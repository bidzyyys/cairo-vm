use num_bigint::BigInt;
use num_integer::Integer;
use num_traits::ToPrimitive;
use std::ops::Shl;

use crate::bigint;
use crate::math_utils::safe_div;
use crate::types::instance_definitions::bitwise_instance_def::{
    BitwiseInstanceDef, CELLS_PER_BITWISE, INPUT_CELLS_PER_BITWISE,
};
use crate::types::relocatable::{MaybeRelocatable, Relocatable};
use crate::vm::errors::memory_errors::MemoryError;
use crate::vm::errors::runner_errors::RunnerError;
use crate::vm::vm_core::VirtualMachine;
use crate::vm::vm_memory::memory::Memory;
use crate::vm::vm_memory::memory_segments::MemorySegmentManager;

#[derive(Debug)]
pub struct BitwiseBuiltinRunner {
    _ratio: u32,
    pub base: isize,
    pub(crate) cells_per_instance: u32,
    pub(crate) n_input_cells: u32,
    bitwise_builtin: BitwiseInstanceDef,
    stop_ptr: Option<usize>,
    pub(crate) _included: bool,
    instances_per_component: u32,
}

impl BitwiseBuiltinRunner {
    pub(crate) fn new(instance_def: &BitwiseInstanceDef, include: bool) -> Self {
        BitwiseBuiltinRunner {
            base: 0,
            _ratio: instance_def.ratio,
            cells_per_instance: CELLS_PER_BITWISE,
            n_input_cells: INPUT_CELLS_PER_BITWISE,
            bitwise_builtin: instance_def.clone(),
            stop_ptr: None,
            _included: include,
            instances_per_component: 1,
        }
    }

    pub fn initialize_segments(
        &mut self,
        segments: &mut MemorySegmentManager,
        memory: &mut Memory,
    ) {
        self.base = segments.add(memory).segment_index
    }

    pub fn initial_stack(&self) -> Vec<MaybeRelocatable> {
        if self._included {
            vec![MaybeRelocatable::from((self.base, 0))]
        } else {
            vec![]
        }
    }

    pub fn base(&self) -> isize {
        self.base
    }

    pub fn add_validation_rule(&self, _memory: &mut Memory) -> Result<(), RunnerError> {
        Ok(())
    }

    pub fn deduce_memory_cell(
        &mut self,
        address: &Relocatable,
        memory: &Memory,
    ) -> Result<Option<MaybeRelocatable>, RunnerError> {
        let index = address
            .offset
            .mod_floor(&(self.cells_per_instance as usize));
        if index == 0 || index == 1 {
            return Ok(None);
        }
        let x_addr = MaybeRelocatable::from((address.segment_index, address.offset - index));
        let y_addr = x_addr.add_usize_mod(1, None);

        let num_x = memory.get(&x_addr);
        let num_y = memory.get(&y_addr);
        if let (Ok(Some(MaybeRelocatable::Int(num_x))), Ok(Some(MaybeRelocatable::Int(num_y)))) = (
            num_x.as_ref().map(|x| x.as_ref().map(|x| x.as_ref())),
            num_y.as_ref().map(|x| x.as_ref().map(|x| x.as_ref())),
        ) {
            let _2_pow_bits = bigint!(1).shl(self.bitwise_builtin.total_n_bits);
            if num_x >= &_2_pow_bits {
                return Err(RunnerError::IntegerBiggerThanPowerOfTwo(
                    x_addr,
                    self.bitwise_builtin.total_n_bits,
                    num_x.clone(),
                ));
            };
            if num_y >= &_2_pow_bits {
                return Err(RunnerError::IntegerBiggerThanPowerOfTwo(
                    y_addr,
                    self.bitwise_builtin.total_n_bits,
                    num_y.clone(),
                ));
            };
            let res = match index {
                2 => Some(MaybeRelocatable::from(num_x & num_y)),
                3 => Some(MaybeRelocatable::from(num_x ^ num_y)),
                4 => Some(MaybeRelocatable::from(num_x | num_y)),
                _ => None,
            };
            return Ok(res);
        }
        Ok(None)
    }

    pub fn get_allocated_memory_units(&self, vm: &VirtualMachine) -> Result<usize, MemoryError> {
        let value = safe_div(&bigint!(vm.current_step), &bigint!(self._ratio))
            .map_err(|_| MemoryError::ErrorCalculatingMemoryUnits)?;
        match (self.cells_per_instance * value).to_usize() {
            Some(result) => Ok(result),
            _ => Err(MemoryError::ErrorCalculatingMemoryUnits),
        }
    }

    pub fn get_memory_segment_addresses(&self) -> (&'static str, (isize, Option<usize>)) {
        ("bitwise", (self.base, self.stop_ptr))
    }

    pub fn get_used_cells(&self, vm: &VirtualMachine) -> Result<usize, MemoryError> {
        let base = self.base();
        vm.segments
            .get_segment_used_size(
                base.try_into()
                    .map_err(|_| MemoryError::AddressInTemporarySegment(base))?,
            )
            .ok_or(MemoryError::MissingSegmentUsedSizes)
    }

    pub fn get_used_cells_and_allocated_size(
        &self,
        vm: &VirtualMachine,
    ) -> Result<(usize, usize), MemoryError> {
        let ratio = self._ratio as usize;
        let cells_per_instance = self.cells_per_instance;
        let min_step = ratio * self.instances_per_component as usize;
        if vm.current_step < min_step {
            Err(MemoryError::InsufficientAllocatedCells)
        } else {
            let used = self.get_used_cells(vm)?;
            let size = (cells_per_instance
                * safe_div(&bigint!(vm.current_step), &bigint!(ratio))
                    .map_err(|_| MemoryError::InsufficientAllocatedCells)?)
            .to_usize()
            .ok_or(MemoryError::InsufficientAllocatedCells)?;
            Ok((used, size))
        }
    }

    pub fn get_used_diluted_check_units(&self, diluted_spacing: u32, diluted_n_bits: u32) -> usize {
        let total_n_bits = self.bitwise_builtin.total_n_bits;
        let mut partition = Vec::with_capacity(total_n_bits as usize);
        for i in (0..total_n_bits).step_by((diluted_spacing * diluted_n_bits) as usize) {
            for j in 0..diluted_spacing {
                if i + j < total_n_bits {
                    partition.push(i + j)
                };
            }
        }
        let partition_lengh = partition.len();
        let num_trimmed = partition
            .into_iter()
            .filter(|elem| elem + diluted_spacing * (diluted_n_bits - 1) + 1 > total_n_bits)
            .count();
        4 * partition_lengh + num_trimmed
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::bigint;
    use crate::vm::errors::memory_errors::MemoryError;
    use crate::vm::{runners::builtin_runner::BuiltinRunner, vm_core::VirtualMachine};
    use crate::{
        hint_processor::builtin_hint_processor::builtin_hint_processor_definition::BuiltinHintProcessor,
        serde::deserialize_program::ReferenceManager, types::program::Program,
        utils::test_utils::*, vm::runners::cairo_runner::CairoRunner,
    };
    use num_bigint::Sign;

    #[test]
    fn get_used_cells_and_allocated_size_test() {
        let builtin = BitwiseBuiltinRunner::new(&BitwiseInstanceDef::new(10), true);

        let mut vm = vm!();

        vm.segments.segment_used_sizes = Some(vec![0]);

        let program = Program {
            builtins: vec![String::from("pedersen")],
            prime: bigint!(17),
            data: vec_data!(
                (4612671182993129469_i64),
                (5189976364521848832_i64),
                (18446744073709551615_i128),
                (5199546496550207487_i64),
                (4612389712311386111_i64),
                (5198983563776393216_i64),
                (2),
                (2345108766317314046_i64),
                (5191102247248822272_i64),
                (5189976364521848832_i64),
                (7),
                (1226245742482522112_i64),
                ((
                    b"3618502788666131213697322783095070105623107215331596699973092056135872020470",
                    10
                )),
                (2345108766317314046_i64)
            ),
            constants: HashMap::new(),
            main: Some(8),
            hints: HashMap::new(),
            reference_manager: ReferenceManager {
                references: Vec::new(),
            },
            identifiers: HashMap::new(),
        };

        let mut cairo_runner = CairoRunner::new(&program, "all").unwrap();

        let hint_processor = BuiltinHintProcessor::new_empty();

        let address = cairo_runner.initialize(&mut vm).unwrap();

        cairo_runner
            .run_until_pc(address, &mut vm, &hint_processor)
            .unwrap();

        assert_eq!(builtin.get_used_cells_and_allocated_size(&vm), Ok((0, 5)));
    }

    #[test]
    fn get_allocated_memory_units() {
        let builtin = BitwiseBuiltinRunner::new(&BitwiseInstanceDef::new(10), true);

        let mut vm = vm!();

        let program = Program {
            builtins: vec![String::from("output"), String::from("bitwise")],
            prime: bigint!(17),
            data: vec_data!(
                (4612671182993129469_i64),
                (5189976364521848832_i64),
                (18446744073709551615_i128),
                (5199546496550207487_i64),
                (4612389712311386111_i64),
                (5198983563776393216_i64),
                (2),
                (2345108766317314046_i64),
                (5191102247248822272_i64),
                (5189976364521848832_i64),
                (7),
                (1226245742482522112_i64),
                ((
                    b"3618502788666131213697322783095070105623107215331596699973092056135872020470",
                    10
                )),
                (2345108766317314046_i64)
            ),
            constants: HashMap::new(),
            main: Some(8),
            hints: HashMap::new(),
            reference_manager: ReferenceManager {
                references: Vec::new(),
            },
            identifiers: HashMap::new(),
        };

        let mut cairo_runner = CairoRunner::new(&program, "all").unwrap();

        let hint_processor = BuiltinHintProcessor::new_empty();

        let address = cairo_runner.initialize(&mut vm).unwrap();

        cairo_runner
            .run_until_pc(address, &mut vm, &hint_processor)
            .unwrap();

        assert_eq!(builtin.get_allocated_memory_units(&vm), Ok(5));
    }

    #[test]
    fn deduce_memory_cell_bitwise_for_preset_memory_valid_and() {
        let memory = memory![((0, 5), 10), ((0, 6), 12), ((0, 7), 0)];
        let mut builtin = BitwiseBuiltinRunner::new(&BitwiseInstanceDef::default(), true);
        let result = builtin.deduce_memory_cell(&Relocatable::from((0, 7)), &memory);
        assert_eq!(result, Ok(Some(MaybeRelocatable::from(bigint!(8)))));
    }

    #[test]
    fn deduce_memory_cell_bitwise_for_preset_memory_valid_xor() {
        let memory = memory![((0, 5), 10), ((0, 6), 12), ((0, 8), 0)];
        let mut builtin = BitwiseBuiltinRunner::new(&BitwiseInstanceDef::default(), true);
        let result = builtin.deduce_memory_cell(&Relocatable::from((0, 8)), &memory);
        assert_eq!(result, Ok(Some(MaybeRelocatable::from(bigint!(6)))));
    }

    #[test]
    fn deduce_memory_cell_bitwise_for_preset_memory_valid_or() {
        let memory = memory![((0, 5), 10), ((0, 6), 12), ((0, 9), 0)];
        let mut builtin = BitwiseBuiltinRunner::new(&BitwiseInstanceDef::default(), true);
        let result = builtin.deduce_memory_cell(&Relocatable::from((0, 9)), &memory);
        assert_eq!(result, Ok(Some(MaybeRelocatable::from(bigint!(14)))));
    }

    #[test]
    fn deduce_memory_cell_bitwise_for_preset_memory_incorrect_offset() {
        let memory = memory![((0, 3), 10), ((0, 4), 12), ((0, 5), 0)];
        let mut builtin = BitwiseBuiltinRunner::new(&BitwiseInstanceDef::default(), true);
        let result = builtin.deduce_memory_cell(&Relocatable::from((0, 5)), &memory);
        assert_eq!(result, Ok(None));
    }

    #[test]
    fn deduce_memory_cell_bitwise_for_preset_memory_no_values_to_operate() {
        let memory = memory![((0, 5), 12), ((0, 7), 0)];
        let mut builtin = BitwiseBuiltinRunner::new(&BitwiseInstanceDef::default(), true);
        let result = builtin.deduce_memory_cell(&Relocatable::from((0, 5)), &memory);
        assert_eq!(result, Ok(None));
    }

    #[test]
    fn get_memory_segment_addresses() {
        let builtin = BitwiseBuiltinRunner::new(&BitwiseInstanceDef::default(), true);

        assert_eq!(
            builtin.get_memory_segment_addresses(),
            ("bitwise", (0, None)),
        );
    }

    #[test]
    fn get_memory_accesses_missing_segment_used_sizes() {
        let builtin = BuiltinRunner::Bitwise(BitwiseBuiltinRunner::new(
            &BitwiseInstanceDef::default(),
            true,
        ));
        let vm = vm!();

        assert_eq!(
            builtin.get_memory_accesses(&vm),
            Err(MemoryError::MissingSegmentUsedSizes),
        );
    }

    #[test]
    fn get_memory_accesses_empty() {
        let builtin = BuiltinRunner::Bitwise(BitwiseBuiltinRunner::new(
            &BitwiseInstanceDef::default(),
            true,
        ));
        let mut vm = vm!();

        vm.segments.segment_used_sizes = Some(vec![0]);
        assert_eq!(builtin.get_memory_accesses(&vm), Ok(vec![]));
    }

    #[test]
    fn get_memory_accesses() {
        let builtin = BuiltinRunner::Bitwise(BitwiseBuiltinRunner::new(
            &BitwiseInstanceDef::default(),
            true,
        ));
        let mut vm = vm!();

        vm.segments.segment_used_sizes = Some(vec![4]);
        assert_eq!(
            builtin.get_memory_accesses(&vm),
            Ok(vec![
                (builtin.base(), 0).into(),
                (builtin.base(), 1).into(),
                (builtin.base(), 2).into(),
                (builtin.base(), 3).into(),
            ]),
        );
    }

    #[test]
    fn get_used_cells_missing_segment_used_sizes() {
        let builtin = BuiltinRunner::Bitwise(BitwiseBuiltinRunner::new(
            &BitwiseInstanceDef::default(),
            true,
        ));
        let vm = vm!();

        assert_eq!(
            builtin.get_used_cells(&vm),
            Err(MemoryError::MissingSegmentUsedSizes)
        );
    }

    #[test]
    fn get_used_cells_empty() {
        let builtin = BuiltinRunner::Bitwise(BitwiseBuiltinRunner::new(
            &BitwiseInstanceDef::default(),
            true,
        ));
        let mut vm = vm!();

        vm.segments.segment_used_sizes = Some(vec![0]);
        assert_eq!(builtin.get_used_cells(&vm), Ok(0));
    }

    #[test]
    fn get_used_cells() {
        let builtin = BuiltinRunner::Bitwise(BitwiseBuiltinRunner::new(
            &BitwiseInstanceDef::default(),
            true,
        ));
        let mut vm = vm!();

        vm.segments.segment_used_sizes = Some(vec![4]);
        assert_eq!(builtin.get_used_cells(&vm), Ok(4));
    }

    #[test]
    fn get_used_diluted_check_units_a() {
        let builtin = BuiltinRunner::Bitwise(BitwiseBuiltinRunner::new(
            &BitwiseInstanceDef::default(),
            true,
        ));
        assert_eq!(builtin.get_used_diluted_check_units(12, 2), 535);
    }

    #[test]
    fn get_used_diluted_check_units_b() {
        let builtin = BuiltinRunner::Bitwise(BitwiseBuiltinRunner::new(
            &BitwiseInstanceDef::default(),
            true,
        ));
        assert_eq!(builtin.get_used_diluted_check_units(30, 56), 150);
    }

    #[test]
    fn get_used_diluted_check_units_c() {
        let builtin = BuiltinRunner::Bitwise(BitwiseBuiltinRunner::new(
            &BitwiseInstanceDef::default(),
            true,
        ));
        assert_eq!(builtin.get_used_diluted_check_units(50, 25), 250);
    }
}