use super::GadgetCaller;
use crate::object::{Array, Object};
use crate::{Environment, Evaluator};
use acvm::acir::circuit::gate::{GadgetCall, GadgetInput, Gate};
use acvm::acir::OPCODE;
use noir_field::FieldElement;
use noirc_frontend::hir_def::expr::HirCallExpression;

use super::RuntimeErrorKind;

pub struct EcdsaSecp256k1Gadget;

impl<F: FieldElement> GadgetCaller<F> for EcdsaSecp256k1Gadget {
    fn name() -> OPCODE {
        OPCODE::EcdsaSecp256k1
    }

    fn call(
        evaluator: &mut Evaluator<F>,
        env: &mut Environment<F>,
        call_expr: HirCallExpression,
    ) -> Result<Object<F>, RuntimeErrorKind> {
        let inputs = EcdsaSecp256k1Gadget::prepare_inputs(evaluator, env, call_expr)?;

        // Prepare output

        // Create a fresh variable which will be the root

        let _verify_witness = evaluator.add_witness_to_cs();
        let _verify_object = Object::from_witness(_verify_witness);

        let _verify_gate = GadgetCall {
            name: OPCODE::EcdsaSecp256k1,
            inputs,
            outputs: vec![_verify_witness],
        };

        evaluator.gates.push(Gate::GadgetCall(_verify_gate));

        Ok(_verify_object)
    }
}

impl EcdsaSecp256k1Gadget {
    fn prepare_inputs<F: FieldElement>(
        evaluator: &mut Evaluator<F>,
        env: &mut Environment<F>,
        mut call_expr: HirCallExpression,
    ) -> Result<Vec<GadgetInput>, RuntimeErrorKind> {
        assert_eq!(call_expr.arguments.len(), 4);

        let pub_key_y = call_expr.arguments.pop().unwrap();
        let pub_key_x = call_expr.arguments.pop().unwrap();
        let message = call_expr.arguments.pop().unwrap();
        let signature = call_expr.arguments.pop().unwrap();

        let signature = Array::from_expression(evaluator, env, &signature)?;
        let message = Array::from_expression(evaluator, env, &message)?;
        let pub_key_x = Array::from_expression(evaluator, env, &pub_key_x)?;
        let pub_key_y = Array::from_expression(evaluator, env, &pub_key_y)?;

        let mut inputs: Vec<GadgetInput> = Vec::new();

        // XXX: Technical debt: refactor so this functionality,
        // is not repeated across many gadgets
        for element in pub_key_x.contents.into_iter() {
            let witness = match element {
                Object::Integer(integer) => (integer.witness),
                Object::Linear(lin) => {
                    if !lin.is_unit() {
                        unimplemented!("logic for non unit witnesses is currently not implemented")
                    }
                    lin.witness
                }
                k => unimplemented!("logic for {:?} is not implemented yet", k),
            };

            inputs.push(GadgetInput {
                witness,
                num_bits: 8,
            });
        }

        for element in pub_key_y.contents.into_iter() {
            let witness = match element {
                Object::Integer(integer) => (integer.witness),
                Object::Linear(lin) => {
                    if !lin.is_unit() {
                        unimplemented!("logic for non unit witnesses is currently not implemented")
                    }
                    lin.witness
                }
                k => unimplemented!("logic for {:?} is not implemented yet", k),
            };

            inputs.push(GadgetInput {
                witness,
                num_bits: 8,
            });
        }

        for element in signature.contents.into_iter() {
            let witness = match element {
                Object::Integer(integer) => (integer.witness),
                Object::Linear(lin) => {
                    if !lin.is_unit() {
                        unimplemented!(" logic for non unit witnesses is currently not implemented")
                    }
                    lin.witness
                }
                k => unimplemented!(" logic for {:?} is not implemented yet", k),
            };

            inputs.push(GadgetInput {
                witness,
                num_bits: 8,
            });
        }
        for element in message.contents.into_iter() {
            let witness = match element {
                Object::Integer(integer) => (integer.witness),
                Object::Linear(lin) => {
                    if !lin.is_unit() {
                        unimplemented!(" logic for non unit witnesses is currently not implemented")
                    }
                    lin.witness
                }
                k => unimplemented!(" logic for {:?} is not implemented yet", k),
            };

            inputs.push(GadgetInput {
                witness,
                num_bits: 8,
            });
        }

        Ok(inputs)
    }
}
