extern crate halo2;

use std::marker::PhantomData;

use halo2::{
    arithmetic::FieldExt,
    circuit::{layouter::SingleChip, Cell, Chip, Layouter},
    dev::VerifyFailure,
    plonk::{
        Advice, Assignment, Circuit, Column, ConstraintSystem, Error, Fixed, Instance, Permutation,
    },
    poly::Rotation,
};

// ANCHOR: instructions
trait NumericInstructions: Chip {
    /// Variable representing a number.
    type Num;

    /// Loads a number into the circuit as a private input.
    fn load_private(
        layouter: &mut impl Layouter<Self>,
        a: Option<Self::Field>,
    ) -> Result<Self::Num, Error>;

    /// Returns `c = a * b`.
    fn mul(
        layouter: &mut impl Layouter<Self>,
        a: Self::Num,
        b: Self::Num,
    ) -> Result<Self::Num, Error>;

    /// Exposes a number as a public input to the circuit.
    fn expose_public(layouter: &mut impl Layouter<Self>, num: Self::Num) -> Result<(), Error>;
}
// ANCHOR_END: instructions

// ANCHOR: chip
/// The chip that will implement our instructions! Chips do not store any persistent
/// state themselves, and usually only contain type markers if necessary.
struct FieldChip<F: FieldExt> {
    _marker: PhantomData<F>,
}
// ANCHOR_END: chip

// ANCHOR: chip-config
/// Chip state is stored in a separate config struct. This is generated by the chip
/// during configuration, and then handed to the `Layouter`, which makes it available
/// to the chip when it needs to implement its instructions.
#[derive(Clone, Debug)]
struct FieldConfig {
    /// For this chip, we will use two advice columns to implement our instructions.
    /// These are also the columns through which we communicate with other parts of
    /// the circuit.
    advice: [Column<Advice>; 2],

    // We need to create a permutation between our advice columns. This allows us to
    // copy numbers within these columns from arbitrary rows, which we can use to load
    // inputs into our instruction regions.
    perm: Permutation,

    // We need a selector to enable the multiplication gate, so that we aren't placing
    // any constraints on cells where `NumericInstructions::mul` is not being used.
    // This is important when building larger circuits, where columns are used by
    // multiple sets of instructions.
    s_mul: Column<Fixed>,

    // The selector for the public-input gate, which uses one of the advice columns.
    s_pub: Column<Fixed>,
}

impl<F: FieldExt> FieldChip<F> {
    fn configure(
        meta: &mut ConstraintSystem<F>,
        advice: [Column<Advice>; 2],
        instance: Column<Instance>,
    ) -> FieldConfig {
        let perm = Permutation::new(
            meta,
            &advice
                .iter()
                .map(|column| (*column).into())
                .collect::<Vec<_>>(),
        );
        let s_mul = meta.fixed_column();
        let s_pub = meta.fixed_column();

        // Define our multiplication gate!
        meta.create_gate("mul", |meta| {
            // To implement multiplication, we need three advice cells and a selector
            // cell. We arrange them like so:
            //
            // | a0  | a1  | s_mul |
            // |-----|-----|-------|
            // | lhs | rhs | s_mul |
            // | out |     |       |
            //
            // Gates may refer to any relative offsets we want, but each distinct
            // offset adds a cost to the proof. The most common offsets are 0 (the
            // current row), 1 (the next row), and -1 (the previous row), for which
            // `Rotation` has specific constructors.
            let lhs = meta.query_advice(advice[0], Rotation::cur());
            let rhs = meta.query_advice(advice[1], Rotation::cur());
            let out = meta.query_advice(advice[0], Rotation::next());
            let s_mul = meta.query_fixed(s_mul, Rotation::cur());

            // The polynomial expression returned from `create_gate` will be
            // constrained by the proving system to equal zero. Our expression
            // has the following properties:
            // - When s_mul = 0, any value is allowed in lhs, rhs, and out.
            // - When s_mul != 0, this constrains lhs * rhs = out.
            s_mul * (lhs * rhs + out * -F::one())
        });

        // Define our public-input gate!
        meta.create_gate("public input", |meta| {
            // We choose somewhat-arbitrarily that we will use the second advice
            // column for exposing numbers as public inputs.
            let a = meta.query_advice(advice[1], Rotation::cur());
            let p = meta.query_instance(instance, Rotation::cur());
            let s = meta.query_fixed(s_pub, Rotation::cur());

            // We simply constrain the advice cell to be equal to the instance cell,
            // when the selector is enabled.
            s * (p + a * -F::one())
        });

        FieldConfig {
            advice,
            perm,
            s_mul,
            s_pub,
        }
    }
}
// ANCHOR_END: chip-config

// ANCHOR: chip-impl
impl<F: FieldExt> Chip for FieldChip<F> {
    type Config = FieldConfig;
    type Field = F;

    fn load(_layouter: &mut impl Layouter<Self>) -> Result<(), halo2::plonk::Error> {
        // None of the instructions implemented by this chip have any fixed state.
        // But if we required e.g. a lookup table, this is where we would load it.
        Ok(())
    }
}
// ANCHOR_END: chip-impl

// ANCHOR: instructions-impl
/// A variable representing a number.
#[derive(Clone)]
struct Number<F: FieldExt> {
    cell: Cell,
    value: Option<F>,
}

impl<F: FieldExt> NumericInstructions for FieldChip<F> {
    type Num = Number<F>;

    fn load_private(
        layouter: &mut impl Layouter<Self>,
        value: Option<Self::Field>,
    ) -> Result<Self::Num, Error> {
        let config = layouter.config().clone();
        let mut num = None;
        layouter.assign_region(
            || "load private",
            |mut region| {
                let cell = region.assign_advice(
                    || "private input",
                    config.advice[0],
                    0,
                    || value.ok_or(Error::SynthesisError),
                )?;
                num = Some(Number { cell, value });
                Ok(())
            },
        )?;
        Ok(num.unwrap())
    }

    fn mul(
        layouter: &mut impl Layouter<Self>,
        a: Self::Num,
        b: Self::Num,
    ) -> Result<Self::Num, Error> {
        let config = layouter.config().clone();
        let mut out = None;
        layouter.assign_region(
            || "mul",
            |mut region| {
                // We only want to use a single multiplication gate in this region,
                // so we enable it at region offset 0; this means it will constrain
                // cells at offsets 0 and 1.
                region.assign_fixed(|| "example mul", config.s_mul, 0, || Ok(F::one()))?;

                // The inputs we've been given could be located anywhere in the circuit,
                // but we can only rely on relative offsets inside this region. So we
                // assign new cells inside the region and constrain them to have the
                // same values as the inputs.
                let lhs = region.assign_advice(
                    || "lhs",
                    config.advice[0],
                    0,
                    || a.value.ok_or(Error::SynthesisError),
                )?;
                let rhs = region.assign_advice(
                    || "rhs",
                    config.advice[1],
                    0,
                    || b.value.ok_or(Error::SynthesisError),
                )?;
                region.constrain_equal(&config.perm, a.cell, lhs)?;
                region.constrain_equal(&config.perm, b.cell, rhs)?;

                // Now we can assign the multiplication result into the output position.
                let value = a.value.and_then(|a| b.value.map(|b| a * b));
                let cell = region.assign_advice(
                    || "lhs * rhs",
                    config.advice[0],
                    1,
                    || value.ok_or(Error::SynthesisError),
                )?;

                // Finally, we return a variable representing the output,
                // to be used in another part of the circuit.
                out = Some(Number { cell, value });
                Ok(())
            },
        )?;

        Ok(out.unwrap())
    }

    fn expose_public(layouter: &mut impl Layouter<Self>, num: Self::Num) -> Result<(), Error> {
        let config = layouter.config().clone();
        layouter.assign_region(
            || "expose public",
            |mut region| {
                // Enable the public-input gate.
                region.assign_fixed(|| "public result", config.s_pub, 0, || Ok(F::one()))?;

                // Load the output into the correct advice column.
                let out = region.assign_advice(
                    || "public advice",
                    config.advice[1],
                    0,
                    || num.value.ok_or(Error::SynthesisError),
                )?;
                region.constrain_equal(&config.perm, num.cell, out)?;

                // We don't assign to the instance column inside the circuit;
                // the mapping of public inputs to cells is provided to the prover.
                Ok(())
            },
        )
    }
}
// ANCHOR_END: instructions-impl

// ANCHOR: circuit
/// The full circuit implementation.
///
/// In this struct we store the private input variables. We use `Option<F>` because
/// they won't have any value during key generation. During proving, if any of these
/// were `None` we would get an error.
struct MyCircuit<F: FieldExt> {
    a: Option<F>,
    b: Option<F>,
}

impl<F: FieldExt> Circuit<F> for MyCircuit<F> {
    // Since we are using a single chip for everything, we can just reuse its config.
    type Config = FieldConfig;

    fn configure(meta: &mut ConstraintSystem<F>) -> Self::Config {
        // We create the two advice columns that FieldChip uses for I/O.
        let advice = [meta.advice_column(), meta.advice_column()];

        // We also need an instance column to store public inputs.
        let instance = meta.instance_column();

        FieldChip::configure(meta, advice, instance)
    }

    fn synthesize(&self, cs: &mut impl Assignment<F>, config: Self::Config) -> Result<(), Error> {
        let mut layouter = SingleChip::new(cs, config);

        // Load our private values into the circuit.
        let a = FieldChip::load_private(&mut layouter, self.a)?;
        let b = FieldChip::load_private(&mut layouter, self.b)?;

        // We only have access to plain multiplication.
        // We could implement our circuit as:
        //     asq = a*a
        //     bsq = b*b
        //     c   = asq*bsq
        //
        // but it's more efficient to implement it as:
        //     ab = a*b
        //     c  = ab^2
        let ab = FieldChip::mul(&mut layouter, a, b)?;
        let c = FieldChip::mul(&mut layouter, ab.clone(), ab)?;

        // Expose the result as a public input to the circuit.
        FieldChip::expose_public(&mut layouter, c)
    }
}
// ANCHOR_END: circuit

fn main() {
    use halo2::{dev::MockProver, pasta::Fp};

    // ANCHOR: test-circuit
    // The number of rows in our circuit cannot exceed 2^k. Since our example
    // circuit is very small, we can pick a very small value here.
    let k = 3;

    // Prepare the private and public inputs to the circuit!
    let a = Fp::from(2);
    let b = Fp::from(3);
    let c = a.square() * b.square();

    // Instantiate the circuit with the private inputs.
    let circuit = MyCircuit {
        a: Some(a),
        b: Some(b),
    };

    // Arrange the public input. We expose the multiplication result in row 6
    // of the instance column, so we position it there in our public inputs.
    let mut public_inputs = vec![Fp::zero(); 1 << k];
    public_inputs[6] = c;

    // Given the correct public input, our circuit will verify.
    let prover = MockProver::run(k, &circuit, vec![public_inputs.clone()]).unwrap();
    assert_eq!(prover.verify(), Ok(()));

    // If we try some other public input, the proof will fail!
    public_inputs[6] += Fp::one();
    let prover = MockProver::run(k, &circuit, vec![public_inputs]).unwrap();
    assert_eq!(
        prover.verify(),
        Err(VerifyFailure::Gate {
            gate_index: 1,
            gate_name: "public input",
            row: 6,
        })
    );
    // ANCHOR_END: test-circuit
}
