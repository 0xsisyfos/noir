//! This module defines the internal slices data fill pass.
//! The purpose of this pass is to fill out nested slice values represented by SSA array values.
//! "Filling out" a nested slice specifically refers to making a nested slice's internal slice types
//! match up in their size. This pass is necessary for dynamic array operations to work in ACIR gen
//! as we need to have a known size for any memory operations. As slice types do not carry a size we
//! need to make sure all nested internal slices have the same size in order to accurately
//! read from or write to a nested slice. This pass ultimately attaches dummy data to any smaller internal slice types.
//!
//! A simple example:
//! If we have a slice of the type [[Field]] which is of length 2. The internal slices themselves
//! could be of different sizes, such as 3 and 4. An array operation on this nested slice would look
//! something like below:
//! array_get [Field 3, [Field 1, Field 1, Field 1], Field 4, [Field 2, Field 2, Field 2, Field 2]], index Field v0
//! Will get translated into a new instruction like such:
//! array_get [Field 3, [Field 1, Field 1, Field 1, Field 0], Field 4, [Field 2, Field 2, Field 2, Field 2]], index Field v0
//!
//!
//! Currently the pass only works on a single flattened block and should only come at the end of SSA right before ACIR generation.
//! The steps of the pass are as follows:
//! - Process each instruction of the block to collect relevant slice size information. We want to find the maximum size that a nested slice
//! potentially could be. Slices can potentially be set to larger array values or used in intrinsics that increase or shorten their size.
//!     - Track all array constants and compute an initial map of their nested slice sizes. The slice sizes map is simply a map of an SSA array value
//!       to its array size and then any child slice values that may exist.
//!     - We also track a map to resolve a starting array constant to its final possible array value. This map is updated on the appropriate instructions
//!       such as ArraySet or any slice intrinsics.
//!     - On an ArrayGet operation add the resulting value as a possible child of the original slice. In SSA we will reuse the same memory block
//!       for the nested slice and must account for an internal slice being fetched and set to a larger value, otherwise we may have an out of bounds error.
//!       Also set the resulting fetched value to have the same internal slice size map as the children of the original array used in the operation.
//!     - On an ArraySet operation we set the resulting value to have the same slice sizes map as the original array used in the operation. Like the result of
//!       an ArrayGet we need to also add the `value` for an ArraySet as a possible child slice of the original array.
//!     - For slice intrinsics we set the resulting value to have the same slice sizes map as the original array the same way as we do in an ArraySet.
//!       However, with a slice intrinsic we also increase the size for the respective slice intrinsics.  
//!       We do not decrement the size on intrinsics that could remove values from a slice. This is because we could potentially go back to the smaller slice size,
//!       not fill in the appropriate dummies and then get an out of bounds error later when executing the ACIR. We always want to compute
//!       what a slice maximum size could be.
//! - Now we need to add each instruction back except with the updated original array values.
//!     - Resolve the original slice value to what its final value would be using the previously computed map.
//!     - Find the max size as each layer of the recursive nested slice type.
//!       For instance in the example above we have a slice of depth 2 with the max sizes of [2, 4].
//!     - Follow the slice type to check whether the SSA value is under the specified max size. If a slice value
//!       is under the max size we then attach dummy data.
//!     - Construct a final nested slice with the now attached dummy data and replace the original array in the previously
//!       saved ArrayGet and ArraySet instructions.

use crate::ssa::{
    ir::{
        basic_block::BasicBlockId,
        dfg::CallStack,
        function::{Function, RuntimeType},
        function_inserter::FunctionInserter,
        instruction::{Instruction, InstructionId},
        post_order::PostOrder,
        types::Type,
        value::{Value, ValueId},
    },
    ssa_gen::Ssa,
};

use acvm::FieldElement;
use fxhash::FxHashMap as HashMap;

use self::capacity_tracker::SliceCapacityTracker;

pub(crate) mod capacity_tracker;

impl Ssa {
    #[tracing::instrument(level = "trace", skip(self))]
    pub(crate) fn fill_internal_slices(mut self) -> Ssa {
        for function in self.functions.values_mut() {
            // This pass is only necessary for generating ACIR and thus we should not
            // process Brillig functions.
            // The pass is also currently only setup to handle a function with a single flattened block.
            // For complex Brillig functions we can expect this pass to panic.
            if function.runtime() == RuntimeType::Acir {
                let databus = function.dfg.data_bus.clone();
                let mut context = Context::new(function);
                context.process_blocks();
                // update the databus with the new array instructions
                function.dfg.data_bus = databus.map_values(|t| context.inserter.resolve(t));
            }
        }
        self
    }
}

struct Context<'f> {
    post_order: PostOrder,
    inserter: FunctionInserter<'f>,

    /// Maps SSA array values representing a slice's contents to its updated array value
    /// after an array set or a slice intrinsic operation.
    /// Maps original value -> result
    mapped_slice_values: HashMap<ValueId, ValueId>,

    /// Maps an updated array value following an array operation to its previous value.
    /// When used in conjunction with `mapped_slice_values` we form a two way map of all array
    /// values being used in array operations.
    /// Maps result -> original value
    slice_parents: HashMap<ValueId, ValueId>,

    // Values containing nested slices to be replaced
    slice_values: Vec<ValueId>,

    // This is set after collecting information from all instructions
    nested_max: Option<usize>,
}

impl<'f> Context<'f> {
    fn new(function: &'f mut Function) -> Self {
        let post_order = PostOrder::with_function(function);
        let inserter = FunctionInserter::new(function);

        Context {
            post_order,
            inserter,
            mapped_slice_values: HashMap::default(),
            slice_parents: HashMap::default(),
            slice_values: Vec::new(),
            nested_max: None,
        }
    }

    fn process_blocks(&mut self) {
        let mut block_order = PostOrder::with_function(self.inserter.function).into_vec();
        block_order.reverse();
        for block in block_order {
            self.process_block(block);
        }
    }

    fn process_block(&mut self, block: BasicBlockId) {
        // Fetch SSA values potentially with internal slices
        let instructions = self.inserter.function.dfg[block].take_instructions();

        // Maps SSA array ID representing slice contents to its length and a list of its potential internal slices
        // This map is constructed once for an array constant and is then updated
        // according to the rules in `collect_slice_information`.
        let mut slice_sizes: HashMap<ValueId, (usize, Vec<ValueId>)> = HashMap::default();

        let mut capacity_tracker = SliceCapacityTracker::new(&self.inserter.function.dfg);
        // Update the slice sizes map to help find the potential max size of each nested slice.
        for instruction in instructions.iter() {
            let results = self.inserter.function.dfg.instruction_results(*instruction).to_vec();
            let instruction = &self.inserter.function.dfg[*instruction];
            capacity_tracker.collect_slice_information(instruction, &mut slice_sizes, results);
        }

        self.slice_values = capacity_tracker.constant_nested_slices();
        self.mapped_slice_values = capacity_tracker.slice_values_map();
        self.slice_parents = capacity_tracker.slice_parents_map();

        // Compute slice nested max
        // Here we are currently assuming the nested max throughout the block
        // TODO: This can be optimized to better track the nested max for specific slices.
        // TODO: Tracking the nested max for each slice was simpler before enabling the merging of nested slices.
        let mut nested_max = 0;
        for (slice_value, size_and_children) in slice_sizes.iter() {
            let typ = self.inserter.function.dfg.type_of_value(*slice_value);
            let depth = Self::compute_nested_slice_depth(&typ);

            let mut max_sizes = Vec::new();
            max_sizes.resize(depth, 0);

            max_sizes[0] = size_and_children.0;
            self.compute_slice_max_sizes(*slice_value, &slice_sizes, &mut max_sizes, 1);

            for size in max_sizes[1..].iter() {
                if *size > nested_max {
                    nested_max = *size;
                }
            }
        }
        self.nested_max = Some(nested_max);

        // Add back every instruction with the updated nested slices.
        for instruction in instructions.iter() {
            self.push_updated_instruction(*instruction, block);
        }

        self.inserter.map_terminator_in_place(block);
    }

    fn push_updated_instruction(&mut self, instruction: InstructionId, block: BasicBlockId) {
        match &self.inserter.function.dfg[instruction] {
            Instruction::ArrayGet { array, .. } | Instruction::ArraySet { array, .. } => {
                if self.slice_values.contains(array) {
                    let (new_array_op_instr, call_stack) =
                        self.get_updated_array_op_instr(*array, instruction);
                    self.inserter.push_instruction_value(
                        new_array_op_instr,
                        instruction,
                        block,
                        call_stack,
                    );
                } else {
                    self.inserter.push_instruction(instruction, block);
                }
            }
            Instruction::Call { arguments, .. } => {
                let mut args_to_replace = Vec::new();
                for (i, arg) in arguments.iter().enumerate() {
                    let element_typ = self.inserter.function.dfg.type_of_value(*arg);
                    if self.slice_values.contains(arg) && element_typ.contains_slice_element() {
                        args_to_replace.push((i, *arg));
                    }
                }
                if args_to_replace.is_empty() {
                    self.inserter.push_instruction(instruction, block);
                } else {
                    for (index, arg) in args_to_replace {
                        let element_typ = self.inserter.function.dfg.type_of_value(arg);
                        let new_array = self.attach_slice_dummies(&element_typ, Some(arg), false);

                        let instruction_id = instruction;
                        let (instruction, call_stack) =
                            self.inserter.map_instruction(instruction_id);
                        let new_call_instr = match instruction {
                            Instruction::Call { func, mut arguments } => {
                                arguments[index] = new_array;
                                Instruction::Call { func, arguments }
                            }
                            _ => panic!("Expected call instruction"),
                        };
                        self.inserter.push_instruction_value(
                            new_call_instr,
                            instruction_id,
                            block,
                            call_stack,
                        );
                    }
                }
            }
            _ => {
                self.inserter.push_instruction(instruction, block);
            }
        }
    }

    /// Construct an updated ArrayGet or ArraySet instruction where the array value
    /// has been replaced by a newly filled in array according to the max internal
    /// slice sizes.
    fn get_updated_array_op_instr(
        &mut self,
        array_id: ValueId,
        instruction: InstructionId,
    ) -> (Instruction, CallStack) {
        let typ = self.inserter.function.dfg.type_of_value(array_id);

        let new_array = self.attach_slice_dummies(&typ, Some(array_id), true);
        let instruction_id = instruction;
        let (instruction, call_stack) = self.inserter.map_instruction(instruction_id);
        let new_array_op_instr = match instruction {
            Instruction::ArrayGet { index, .. } => {
                Instruction::ArrayGet { array: new_array, index }
            }
            Instruction::ArraySet { index, value, .. } => {
                Instruction::ArraySet { array: new_array, index, value }
            }
            _ => panic!("Expected array set"),
        };

        (new_array_op_instr, call_stack)
    }

    fn attach_slice_dummies(
        &mut self,
        typ: &Type,
        value: Option<ValueId>,
        is_parent_slice: bool,
    ) -> ValueId {
        match typ {
            Type::Numeric(_) => {
                if let Some(value) = value {
                    self.inserter.resolve(value)
                } else {
                    let zero = FieldElement::zero();
                    self.inserter.function.dfg.make_constant(zero, Type::field())
                }
            }
            Type::Array(element_types, len) => {
                if let Some(value) = value {
                    self.inserter.resolve(value)
                } else {
                    let mut array = im::Vector::new();
                    for _ in 0..*len {
                        for typ in element_types.iter() {
                            array.push_back(self.attach_slice_dummies(typ, None, false));
                        }
                    }
                    self.inserter.function.dfg.make_array(array, typ.clone())
                }
            }
            Type::Slice(element_types) => {
                let mut max_size = self
                    .nested_max
                    .expect("ICE: should have nested max when attaching slice dummy data");

                if let Some(value_id) = value {
                    let mut slice = im::Vector::new();

                    let value = self.inserter.function.dfg[value_id].clone();
                    let array = match value {
                        Value::Array { array, .. } => array,
                        _ => {
                            panic!("Expected an array value");
                        }
                    };

                    if is_parent_slice {
                        max_size = array.len() / element_types.len();
                    }

                    for i in 0..max_size {
                        for (element_index, element_type) in element_types.iter().enumerate() {
                            let index_usize = i * element_types.len() + element_index;
                            let valid_index = index_usize < array.len();
                            let maybe_value =
                                if valid_index { Some(array[index_usize]) } else { None };
                            slice.push_back(self.attach_slice_dummies(
                                element_type,
                                maybe_value,
                                false,
                            ));
                        }
                    }

                    self.inserter.function.dfg.make_array(slice, typ.clone())
                } else {
                    let mut slice = im::Vector::new();
                    for _ in 0..max_size {
                        for typ in element_types.iter() {
                            slice.push_back(self.attach_slice_dummies(typ, None, false));
                        }
                    }
                    self.inserter.function.dfg.make_array(slice, typ.clone())
                }
            }
            Type::Reference(_) => {
                unreachable!("ICE: Generating dummy data for references is unsupported")
            }
            Type::Function => {
                unreachable!("ICE: Generating dummy data for functions is unsupported")
            }
        }
    }

    /// Determine the maximum possible size of an internal slice at each
    /// layer of a nested slice.
    ///
    /// If the slice map is incorrectly formed the function will exceed
    /// the type's nested slice depth and panic.
    fn compute_slice_max_sizes(
        &self,
        array_id: ValueId,
        slice_sizes: &HashMap<ValueId, (usize, Vec<ValueId>)>,
        max_sizes: &mut Vec<usize>,
        depth: usize,
    ) {
        let array_id = self.resolve_slice_value(array_id);
        let (current_size, inner_slices) = slice_sizes
            .get(&array_id)
            .unwrap_or_else(|| panic!("should have slice sizes: {array_id}"));

        if inner_slices.is_empty() {
            return;
        }

        let mut max = *current_size;
        for inner_slice in inner_slices.iter() {
            let inner_slice = &self.resolve_slice_value(*inner_slice);

            let (inner_size, _) = slice_sizes[inner_slice];
            if inner_size > max {
                max = inner_size;
            }
            self.compute_slice_max_sizes(*inner_slice, slice_sizes, max_sizes, depth + 1);
        }

        if max > max_sizes[depth] {
            max_sizes[depth] = max;
        }
    }

    /// Compute the depth of nested slices in a given Type.
    /// The depth follows the recursive type structure of a slice.
    fn compute_nested_slice_depth(typ: &Type) -> usize {
        let mut depth = 0;
        match typ {
            Type::Slice(element_types) => {
                depth += 1;
                for typ in element_types.as_ref() {
                    depth += Self::compute_nested_slice_depth(typ);
                }
            }
            Type::Reference(element) => {
                depth += Self::compute_nested_slice_depth(element);
            }
            Type::Array(element_types, _) => {
                for typ in element_types.as_ref() {
                    depth += Self::compute_nested_slice_depth(typ);
                }
            }
            _ => {
                // Do nothing
            }
        }
        depth
    }

    /// Resolves a ValueId representing a slice's contents to its updated value.
    /// If there is no resolved value for the supplied value, the value which
    /// was passed to the method is returned.
    fn resolve_slice_value(&self, array_id: ValueId) -> ValueId {
        match self.mapped_slice_values.get(&array_id) {
            Some(value) => self.resolve_slice_value(*value),
            None => array_id,
        }
    }

    /// Resolves a ValueId representing a slice's contents to its previous value.
    /// If there is no resolved parent value it means we have the original slice value
    /// and the value which was passed to the method is returned.
    fn resolve_slice_parent(&self, array_id: ValueId) -> ValueId {
        match self.slice_parents.get(&array_id) {
            Some(value) => self.resolve_slice_parent(*value),
            None => array_id,
        }
    }
}

#[cfg(test)]
mod tests {

    use std::rc::Rc;

    use acvm::FieldElement;
    use im::vector;

    use crate::ssa::{
        function_builder::FunctionBuilder,
        ir::{
            dfg::DataFlowGraph,
            function::RuntimeType,
            instruction::{BinaryOp, Instruction},
            map::Id,
            types::Type,
            value::ValueId,
        },
    };

    #[test]
    fn test_simple_nested_slice() {
        // We want to test that a nested slice with two internal slices of primitive types
        // fills the smaller internal slice with dummy data to match the length of the
        // larger internal slice.

        // Note that slices are a represented by a tuple of (length, contents).
        // The type of the nested slice in this test is [[Field]].
        //
        // This is the original SSA:
        // acir fn main f0 {
        //     b0(v0: Field):
        //       v2 = lt v0, Field 2
        //       constrain v2 == Field 1 'Index out of bounds'
        //       v11 = array_get [[Field 3, [Field 1, Field 1, Field 1]], [Field 4, [Field 2, Field 2, Field 2, Field 2]]], index Field v0
        //       constrain v11 == Field 4
        //       return
        // }

        let main_id = Id::test_new(0);
        let mut builder = FunctionBuilder::new("main".into(), main_id, RuntimeType::Acir);

        let main_v0 = builder.add_parameter(Type::field());

        let two = builder.field_constant(2_u128);
        // Every slice access checks against the dynamic slice length
        let slice_access_check = builder.insert_binary(main_v0, BinaryOp::Lt, two);
        let one = builder.field_constant(1_u128);
        builder.insert_constrain(slice_access_check, one, Some("Index out of bounds".to_owned()));

        let field_element_type = Rc::new(vec![Type::field()]);
        let inner_slice_contents_type = Type::Slice(field_element_type);

        let inner_slice_small_len = builder.field_constant(3_u128);
        let inner_slice_small_contents =
            builder.array_constant(vector![one, one, one], inner_slice_contents_type.clone());

        let inner_slice_big_len = builder.field_constant(4_u128);
        let inner_slice_big_contents =
            builder.array_constant(vector![two, two, two, two], inner_slice_contents_type.clone());

        let outer_slice_element_type = Rc::new(vec![Type::field(), inner_slice_contents_type]);
        let outer_slice_type = Type::Slice(outer_slice_element_type);

        let outer_slice_contents = builder.array_constant(
            vector![
                inner_slice_small_len,
                inner_slice_small_contents,
                inner_slice_big_len,
                inner_slice_big_contents
            ],
            outer_slice_type,
        );
        // Fetching the length of the second nested slice
        // We must use a parameter to main as we do not want the array operation to be simplified out during SSA gen. The filling of internal slices
        // is necessary for dynamic nested slices and thus we want to generate the SSA that ACIR gen would be converting.
        let array_get_res = builder.insert_array_get(outer_slice_contents, main_v0, Type::field());

        let four = builder.field_constant(4_u128);
        builder.insert_constrain(array_get_res, four, None);
        builder.terminate_with_return(vec![]);

        // Note that now the smaller internal slice should have extra dummy data that matches the larger internal slice's size.
        //
        // Expected SSA:
        // acir fn main f0 {
        //     b0(v0: Field):
        //       v10 = lt v0, Field 2
        //       constrain v10 == Field 1 'Index out of bounds'
        //       v18 = array_get [Field 3, [Field 1, Field 1, Field 1, Field 0], Field 4, [Field 2, Field 2, Field 2, Field 2]], index v0
        //       constrain v18 == Field 4
        //       return
        // }

        let ssa = builder.finish().fill_internal_slices();

        let func = ssa.main();
        let block_id = func.entry_block();

        // Check the array get expression has replaced its nested slice with a new slice
        // where the internal slice has dummy data attached to it.
        let instructions = func.dfg[block_id].instructions();
        let array_id = instructions
            .iter()
            .find_map(|instruction| {
                if let Instruction::ArrayGet { array, .. } = func.dfg[*instruction] {
                    Some(array)
                } else {
                    None
                }
            })
            .expect("Should find array_get instruction");

        let (array_constant, _) =
            func.dfg.get_array_constant(array_id).expect("should have an array constant");

        let inner_slice_small_len = func
            .dfg
            .get_numeric_constant(array_constant[0])
            .expect("should have a numeric constant");
        assert_eq!(
            inner_slice_small_len,
            FieldElement::from(3u128),
            "The length of the smaller internal slice should be unchanged"
        );

        let (inner_slice_small_contents, _) =
            func.dfg.get_array_constant(array_constant[1]).expect("should have an array constant");
        let small_capacity = inner_slice_small_contents.len();
        assert_eq!(small_capacity, 4, "The inner slice contents should contain dummy element");

        compare_array_constants(&inner_slice_small_contents, &[1, 1, 1, 0], &func.dfg);

        let inner_slice_big_len = func
            .dfg
            .get_numeric_constant(array_constant[2])
            .expect("should have a numeric constant");
        assert_eq!(
            inner_slice_big_len,
            FieldElement::from(4u128),
            "The length of the larger internal slice should be unchanged"
        );

        let (inner_slice_big_contents, _) =
            func.dfg.get_array_constant(array_constant[3]).expect("should have an array constant");
        let big_capacity = inner_slice_big_contents.len();
        assert_eq!(
            small_capacity, big_capacity,
            "The length of both internal slice contents should be the same"
        );

        compare_array_constants(&inner_slice_big_contents, &[2u128; 4], &func.dfg);
    }

    fn compare_array_constants(
        got_list: &im::Vector<ValueId>,
        expected_list: &[u128],
        dfg: &DataFlowGraph,
    ) {
        for i in 0..got_list.len() {
            let got_value =
                dfg.get_numeric_constant(got_list[i]).expect("should have a numeric constant");
            assert_eq!(
                got_value,
                FieldElement::from(expected_list[i]),
                "Value is different than expected"
            );
        }
    }
}
