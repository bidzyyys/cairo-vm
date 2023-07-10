use crate::math_utils::{ec_add, ec_double};
use crate::stdlib::{borrow::Cow, prelude::*};
use crate::stdlib::{cell::RefCell, collections::HashMap};
use crate::types::instance_definitions::ec_op_instance_def::{
    EcOpInstanceDef, CELLS_PER_EC_OP, INPUT_CELLS_PER_EC_OP,
};
use crate::types::relocatable::{MaybeRelocatable, Relocatable};
use crate::utils::{bigint_to_felt, felt_to_bigint, CAIRO_PRIME};
use crate::vm::errors::memory_errors::MemoryError;
use crate::vm::errors::runner_errors::RunnerError;
use crate::vm::vm_memory::memory::Memory;
use crate::vm::vm_memory::memory_segments::MemorySegmentManager;
use crate::Felt252;
use num_bigint::{BigInt, ToBigInt};
use num_integer::{div_ceil, Integer};
use num_traits::{One, Zero};

use super::EC_OP_BUILTIN_NAME;

#[derive(Debug, Clone)]
pub struct EcOpBuiltinRunner {
    ratio: Option<u32>,
    pub base: usize,
    pub(crate) cells_per_instance: u32,
    pub(crate) n_input_cells: u32,
    ec_op_builtin: EcOpInstanceDef,
    pub(crate) stop_ptr: Option<usize>,
    pub(crate) included: bool,
    pub(crate) instances_per_component: u32,
    cache: RefCell<HashMap<Relocatable, Felt252>>,
}

impl EcOpBuiltinRunner {
    pub(crate) fn new(instance_def: &EcOpInstanceDef, included: bool) -> Self {
        EcOpBuiltinRunner {
            base: 0,
            ratio: instance_def.ratio,
            n_input_cells: INPUT_CELLS_PER_EC_OP,
            cells_per_instance: CELLS_PER_EC_OP,
            ec_op_builtin: instance_def.clone(),
            stop_ptr: None,
            included,
            instances_per_component: 1,
            cache: RefCell::new(HashMap::new()),
        }
    }
    ///Returns True if the point (x, y) is on the elliptic curve defined as
    ///y^2 = x^3 + alpha * x + beta (mod p)
    ///or False otherwise.
    fn point_on_curve(x: &Felt252, y: &Felt252, alpha: &Felt252, beta: &Felt252) -> bool {
        y.pow(2) == &(x.pow(3) + alpha * x) + beta
    }

    #[allow(deprecated)]
    ///Returns the result of the EC operation P + m * Q.
    /// where P = (p_x, p_y), Q = (q_x, q_y) are points on the elliptic curve defined as
    /// y^2 = x^3 + alpha * x + beta (mod prime).
    /// Mimics the operation of the AIR, so that this function fails whenever the builtin AIR
    /// would not yield a correct result, i.e. when any part of the computation attempts to add
    /// two points with the same x coordinate.
    fn ec_op_impl(
        partial_sum: (Felt252, Felt252),
        doubled_point: (Felt252, Felt252),
        m: &Felt252,
        alpha: &BigInt,
        prime: &BigInt,
        height: u32,
    ) -> Result<(BigInt, BigInt), RunnerError> {
        let mut slope = felt_to_bigint(*m);
        let mut partial_sum_b = (felt_to_bigint(partial_sum.0), felt_to_bigint(partial_sum.1));
        let mut doubled_point_b = (
            felt_to_bigint(doubled_point.0),
            felt_to_bigint(doubled_point.1),
        );
        for _ in 0..height {
            if (doubled_point_b.0.clone() - partial_sum_b.0.clone()).is_zero() {
                #[allow(deprecated)]
                return Err(RunnerError::EcOpSameXCoordinate(
                    Self::format_ec_op_error(partial_sum_b, slope, doubled_point_b)
                        .into_boxed_str(),
                ));
            };
            if !(slope.clone() & &BigInt::one()).is_zero() {
                partial_sum_b = ec_add(partial_sum_b, doubled_point_b.clone(), prime);
            }
            doubled_point_b = ec_double(doubled_point_b, alpha, prime);
            slope = slope.clone() >> 1_u32;
        }
        Ok(partial_sum_b)
    }

    pub fn initialize_segments(&mut self, segments: &mut MemorySegmentManager) {
        self.base = segments.add().segment_index as usize // segments.add() always returns a positive index
    }

    pub fn initial_stack(&self) -> Vec<MaybeRelocatable> {
        if self.included {
            vec![MaybeRelocatable::from((self.base as isize, 0))]
        } else {
            vec![]
        }
    }

    pub fn base(&self) -> usize {
        self.base
    }

    pub fn ratio(&self) -> Option<u32> {
        self.ratio
    }

    pub fn add_validation_rule(&self, _memory: &mut Memory) {}

    pub fn deduce_memory_cell(
        &self,
        address: Relocatable,
        memory: &Memory,
    ) -> Result<Option<MaybeRelocatable>, RunnerError> {
        //Constant values declared here
        const EC_POINT_INDICES: [(usize, usize); 3] = [(0, 1), (2, 3), (5, 6)];
        const OUTPUT_INDICES: (usize, usize) = EC_POINT_INDICES[2];
        let alpha: Felt252 = Felt252::ONE;
        let beta_low: Felt252 = Felt252::from(0x609ad26c15c915c1f4cdfcb99cee9e89_u128);
        let beta_high: Felt252 = Felt252::from(0x6f21413efbe40de150e596d72f7a8c5_u128);
        let beta: Felt252 = (beta_high << 128_usize) + beta_low;

        let index = address
            .offset
            .mod_floor(&(self.cells_per_instance as usize));
        //Index should be an output cell
        if index != OUTPUT_INDICES.0 && index != OUTPUT_INDICES.1 {
            return Ok(None);
        }
        let instance = Relocatable::from((address.segment_index, address.offset - index));
        let x_addr = (instance + (&Felt252::from(INPUT_CELLS_PER_EC_OP)))
            .map_err(|_| RunnerError::Memory(MemoryError::ExpectedInteger(Box::new(instance))))?;

        if let Some(number) = self.cache.borrow().get(&address).cloned() {
            return Ok(Some(MaybeRelocatable::Int(number)));
        }

        //All input cells should be filled, and be integer values
        //If an input cell is not filled, return None
        let mut input_cells = Vec::<&Felt252>::with_capacity(self.n_input_cells as usize);
        for i in 0..self.n_input_cells as usize {
            match memory.get(&(instance + i)?) {
                None => return Ok(None),
                Some(addr) => {
                    input_cells.push(match addr {
                        // Only relocatable values can be owned
                        Cow::Borrowed(MaybeRelocatable::Int(ref num)) => num,
                        _ => {
                            return Err(RunnerError::Memory(MemoryError::ExpectedInteger(
                                Box::new((instance + i)?),
                            )))
                        }
                    });
                }
            };
        }
        //Assert that m is under the limit defined by scalar_limit.
        /*if input_cells[M_INDEX].as_ref() >= &self.ec_op_builtin.scalar_limit {
            return Err(RunnerError::EcOpBuiltinScalarLimit(
                self.ec_op_builtin.scalar_limit.clone(),
            ));
        }*/

        // Assert that if the current address is part of a point, the point is on the curve
        for pair in &EC_POINT_INDICES[0..2] {
            if !EcOpBuiltinRunner::point_on_curve(
                input_cells[pair.0],
                input_cells[pair.1],
                &alpha,
                &beta,
            ) {
                return Err(RunnerError::PointNotOnCurve(Box::new((
                    input_cells[pair.0].clone(),
                    input_cells[pair.1].clone(),
                ))));
            };
        }
        let prime = CAIRO_PRIME.to_bigint().unwrap();
        let result = EcOpBuiltinRunner::ec_op_impl(
            (input_cells[0].to_owned(), input_cells[1].to_owned()),
            (input_cells[2].to_owned(), input_cells[3].to_owned()),
            input_cells[4],
            #[allow(deprecated)]
            &felt_to_bigint(alpha),
            &prime,
            self.ec_op_builtin.scalar_height,
        )?;
        let result = (bigint_to_felt(&result.0)?, bigint_to_felt(&result.1)?);
        self.cache.borrow_mut().insert(x_addr, result.0);
        self.cache.borrow_mut().insert(
            (x_addr + 1usize)
                .map_err(|_| RunnerError::Memory(MemoryError::ExpectedInteger(Box::new(x_addr))))?,
            result.1,
        );
        match index - self.n_input_cells as usize {
            0 => Ok(Some(MaybeRelocatable::Int(result.0))),
            _ => Ok(Some(MaybeRelocatable::Int(result.1))),
            //Default case corresponds to 1, as there are no other possible cases
        }
    }

    pub fn get_memory_segment_addresses(&self) -> (usize, Option<usize>) {
        (self.base, self.stop_ptr)
    }

    pub fn get_used_cells(&self, segments: &MemorySegmentManager) -> Result<usize, MemoryError> {
        segments
            .get_segment_used_size(self.base())
            .ok_or(MemoryError::MissingSegmentUsedSizes)
    }

    pub fn get_used_instances(
        &self,
        segments: &MemorySegmentManager,
    ) -> Result<usize, MemoryError> {
        let used_cells = self.get_used_cells(segments)?;
        Ok(div_ceil(used_cells, self.cells_per_instance as usize))
    }

    pub fn final_stack(
        &mut self,
        segments: &MemorySegmentManager,
        pointer: Relocatable,
    ) -> Result<Relocatable, RunnerError> {
        if self.included {
            let stop_pointer_addr = (pointer - 1)
                .map_err(|_| RunnerError::NoStopPointer(Box::new(EC_OP_BUILTIN_NAME)))?;
            let stop_pointer = segments
                .memory
                .get_relocatable(stop_pointer_addr)
                .map_err(|_| RunnerError::NoStopPointer(Box::new(EC_OP_BUILTIN_NAME)))?;
            if self.base as isize != stop_pointer.segment_index {
                return Err(RunnerError::InvalidStopPointerIndex(Box::new((
                    EC_OP_BUILTIN_NAME,
                    stop_pointer,
                    self.base,
                ))));
            }
            let stop_ptr = stop_pointer.offset;
            let num_instances = self.get_used_instances(segments)?;
            let used = num_instances * self.cells_per_instance as usize;
            if stop_ptr != used {
                return Err(RunnerError::InvalidStopPointer(Box::new((
                    EC_OP_BUILTIN_NAME,
                    Relocatable::from((self.base as isize, used)),
                    Relocatable::from((self.base as isize, stop_ptr)),
                ))));
            }
            self.stop_ptr = Some(stop_ptr);
            Ok(stop_pointer_addr)
        } else {
            let stop_ptr = self.base;
            self.stop_ptr = Some(stop_ptr);
            Ok(pointer)
        }
    }

    pub fn format_ec_op_error(
        p: (num_bigint::BigInt, num_bigint::BigInt),
        m: num_bigint::BigInt,
        q: (num_bigint::BigInt, num_bigint::BigInt),
    ) -> String {
        format!("Cannot apply EC operation: computation reached two points with the same x coordinate. \n
    Attempting to compute P + m * Q where:\n
    P = {p:?} \n
    m = {m:?}\n
    Q = {q:?}.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hint_processor::builtin_hint_processor::builtin_hint_processor_definition::BuiltinHintProcessor;
    use crate::serde::deserialize_program::BuiltinName;
    use crate::stdlib::collections::HashMap;
    use crate::types::program::Program;
    use crate::utils::{test_utils::*, CAIRO_PRIME};
    use crate::vm::errors::cairo_run_errors::CairoRunError;
    use crate::vm::errors::vm_errors::VirtualMachineError;
    use crate::vm::runners::cairo_runner::CairoRunner;
    use crate::{felt_hex, relocatable};

    use crate::vm::{
        errors::{memory_errors::MemoryError, runner_errors::RunnerError},
        runners::builtin_runner::BuiltinRunner,
        vm_core::VirtualMachine,
    };
    use EcOpBuiltinRunner;

    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::*;

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn get_used_instances() {
        let builtin = EcOpBuiltinRunner::new(&EcOpInstanceDef::new(Some(10)), true);

        let mut vm = vm!();
        vm.segments.segment_used_sizes = Some(vec![1]);

        assert_eq!(builtin.get_used_instances(&vm.segments), Ok(1));
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn final_stack() {
        let mut builtin = EcOpBuiltinRunner::new(&EcOpInstanceDef::new(Some(10)), true);

        let mut vm = vm!();

        vm.segments = segments![
            ((0, 0), (0, 0)),
            ((0, 1), (0, 1)),
            ((2, 0), (0, 0)),
            ((2, 1), (0, 0))
        ];

        vm.segments.segment_used_sizes = Some(vec![0]);

        let pointer = Relocatable::from((2, 2));

        assert_eq!(
            builtin.final_stack(&vm.segments, pointer).unwrap(),
            Relocatable::from((2, 1))
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn final_stack_error_stop_pointer() {
        let mut builtin = EcOpBuiltinRunner::new(&EcOpInstanceDef::new(Some(10)), true);

        let mut vm = vm!();

        vm.segments = segments![
            ((0, 0), (0, 0)),
            ((0, 1), (0, 1)),
            ((2, 0), (0, 0)),
            ((2, 1), (0, 0))
        ];

        vm.segments.segment_used_sizes = Some(vec![994]);

        let pointer = Relocatable::from((2, 2));

        assert_eq!(
            builtin.final_stack(&vm.segments, pointer),
            Err(RunnerError::InvalidStopPointer(Box::new((
                EC_OP_BUILTIN_NAME,
                relocatable!(0, 994),
                relocatable!(0, 0)
            ))))
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn final_stack_error_when_notincluded() {
        let mut builtin = EcOpBuiltinRunner::new(&EcOpInstanceDef::new(Some(10)), false);

        let mut vm = vm!();

        vm.segments = segments![
            ((0, 0), (0, 0)),
            ((0, 1), (0, 1)),
            ((2, 0), (0, 0)),
            ((2, 1), (0, 0))
        ];

        vm.segments.segment_used_sizes = Some(vec![0]);

        let pointer = Relocatable::from((2, 2));

        assert_eq!(
            builtin.final_stack(&vm.segments, pointer).unwrap(),
            Relocatable::from((2, 2))
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn final_stack_error_non_relocatable() {
        let mut builtin = EcOpBuiltinRunner::new(&EcOpInstanceDef::new(Some(10)), true);

        let mut vm = vm!();

        vm.segments = segments![
            ((0, 0), (0, 0)),
            ((0, 1), (0, 1)),
            ((2, 0), (0, 0)),
            ((2, 1), 2)
        ];

        vm.segments.segment_used_sizes = Some(vec![0]);

        let pointer = Relocatable::from((2, 2));

        assert_eq!(
            builtin.final_stack(&vm.segments, pointer),
            Err(RunnerError::NoStopPointer(Box::new(EC_OP_BUILTIN_NAME)))
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn get_used_cells_and_allocated_size_test() {
        let builtin: BuiltinRunner =
            EcOpBuiltinRunner::new(&EcOpInstanceDef::new(Some(10)), true).into();

        let mut vm = vm!();

        vm.segments.segment_used_sizes = Some(vec![0]);

        let program = program!(
            builtins = vec![BuiltinName::pedersen],
            data = vec_data!(
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
                    "3618502788666131213697322783095070105623107215331596699973092056135872020470",
                    10
                )),
                (2345108766317314046_i64)
            ),
            main = Some(8),
        );
        let mut cairo_runner = cairo_runner!(program);

        let mut hint_processor = BuiltinHintProcessor::new_empty();

        let address = cairo_runner.initialize(&mut vm).unwrap();

        cairo_runner
            .run_until_pc(address, &mut vm, &mut hint_processor)
            .unwrap();

        assert_eq!(builtin.get_used_cells_and_allocated_size(&vm), Ok((0, 7)));
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn get_allocated_memory_units() {
        let builtin: BuiltinRunner =
            EcOpBuiltinRunner::new(&EcOpInstanceDef::new(Some(10)), true).into();

        let mut vm = vm!();

        let program = program!(
            builtins = vec![BuiltinName::ec_op],
            data = vec_data!(
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
                    "3618502788666131213697322783095070105623107215331596699973092056135872020470",
                    10
                )),
                (2345108766317314046_i64)
            ),
            main = Some(8),
        );

        let mut cairo_runner = cairo_runner!(program);

        let mut hint_processor = BuiltinHintProcessor::new_empty();

        let address = cairo_runner.initialize(&mut vm).unwrap();

        cairo_runner
            .run_until_pc(address, &mut vm, &mut hint_processor)
            .unwrap();

        assert_eq!(builtin.get_allocated_memory_units(&vm), Ok(7));
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn point_is_on_curve_a() {
        let x = felt_hex!("0x1ef15c18599971b7beced415a40f0c7deacfd9b0d1819e03d723d8bc943cfca");
        let y = felt_hex!("0x5668060aa49730b7be4801df46ec62de53ecd11abe43a32873000c36e8dc1f");
        let alpha = Felt252::ONE;
        let beta = felt_hex!("0x6f21413efbe40de150e596d72f7a8c5609ad26c15c915c1f4cdfcb99cee9e89");
        assert!(EcOpBuiltinRunner::point_on_curve(&x, &y, &alpha, &beta));
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn point_is_on_curve_b() {
        let x = felt_hex!("0x6f0a1ddaf19c44781c8946db396f494a10ffab183c2d8cf6c4cd321a8d87fd9");
        let y = felt_hex!("0x4afa52a9ef8c023d3385fddb6e1d78d57b0693b9b02d45d0f939b526d474c39");
        let alpha = Felt252::ONE;
        let beta = felt_hex!("0x6f21413efbe40de150e596d72f7a8c5609ad26c15c915c1f4cdfcb99cee9e89");
        assert!(EcOpBuiltinRunner::point_on_curve(&x, &y, &alpha, &beta));
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn point_is_not_on_curve_a() {
        let x = felt_hex!("0x1ef15c1a2162fb0d2e5d83196a6fb0509632fab5d746f0c3d723d8bc943cfca");
        let y = felt_hex!("0x5668060aa49730b7be4801df46ec62de53ecd11abe43a32873000c36e8dc1f");
        let alpha = Felt252::ONE;
        let beta = felt_hex!("0x6f21413efbe40de150e596d72f7a8c5609ad26c15c915c1f4cdfcb99cee9e89");
        assert!(!EcOpBuiltinRunner::point_on_curve(&x, &y, &alpha, &beta));
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn point_is_not_on_curve_b() {
        let x = felt_hex!("0x6f0a1ddaeb88837dcc8ac9a48f894deed706bc3e8998e63535e2c91a8d87fd9");
        let y = felt_hex!("0x4afa52a9ef8c023d33ea3865fb4e0e49abfc50dd50ccea867539b526d474c39");
        let alpha = Felt252::ONE;
        let beta = felt_hex!("0x6f21413efbe40de150e596d72f7a8c5609ad26c15c915c1f4cdfcb99cee9e89");
        assert!(!EcOpBuiltinRunner::point_on_curve(&x, &y, &alpha, &beta));
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn compute_ec_op_impl_valid_a() {
        let partial_sum = (
            felt_hex!("0x6f0a1ddaf19c44781c8946db396f494a10ffab183c2d8cf6c4cd321a8d87fd9"),
            felt_hex!("0x4afa52a9ef8c023d3385fddb6e1d78d57b0693b9b02d45d0f939b526d474c39"),
        );
        let doubled_point = (
            felt_hex!("0x1ef15c18599971b7beced415a40f0c7deacfd9b0d1819e03d723d8bc943cfca"),
            felt_hex!("0x5668060aa49730b7be4801df46ec62de53ecd11abe43a32873000c36e8dc1f"),
        );
        let m = Felt252::from(34);
        let alpha = bigint!(1);
        let height = 256;
        let prime = (*CAIRO_PRIME).clone().into();
        let result =
            EcOpBuiltinRunner::ec_op_impl(partial_sum, doubled_point, &m, &alpha, &prime, height);
        assert_eq!(
            result,
            Ok((
                bigint_str!(
                    "1977874238339000383330315148209250828062304908491266318460063803060754089297"
                ),
                bigint_str!(
                    "2969386888251099938335087541720168257053975603483053253007176033556822156706"
                )
            ))
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn compute_ec_op_impl_valid_b() {
        let partial_sum = (
            felt_hex!("0x68caa9509b7c2e90b4d92661cbf7c465471c1e8598c5f989691eef6653e0f38"),
            felt_hex!("0x79a8673f498531002fc549e06ff2010ffc0c191cceb7da5532acb95cdcb591"),
        );
        let doubled_point = (
            felt_hex!("0x1ef15c18599971b7beced415a40f0c7deacfd9b0d1819e03d723d8bc943cfca"),
            felt_hex!("0x5668060aa49730b7be4801df46ec62de53ecd11abe43a32873000c36e8dc1f"),
        );
        let m = Felt252::from(34);
        let alpha = bigint!(1);
        let height = 256;
        let prime = (*CAIRO_PRIME).clone().into();
        let result =
            EcOpBuiltinRunner::ec_op_impl(partial_sum, doubled_point, &m, &alpha, &prime, height);
        assert_eq!(
            result,
            Ok((
                bigint_str!(
                    "2778063437308421278851140253538604815869848682781135193774472480292420096757"
                ),
                bigint_str!(
                    "3598390311618116577316045819420613574162151407434885460365915347732568210029"
                )
            ))
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    #[allow(deprecated)]
    fn compute_ec_op_invalid_same_x_coordinate() {
        let partial_sum = (Felt252::ONE, Felt252::from(9));
        let doubled_point = (Felt252::ONE, Felt252::from(12));
        let m = Felt252::from(34);
        let alpha = bigint!(1);
        let height = 256;
        let prime = (*CAIRO_PRIME).clone().into();
        let result = EcOpBuiltinRunner::ec_op_impl(
            partial_sum.clone(),
            doubled_point.clone(),
            &m,
            &alpha,
            &prime,
            height,
        );
        assert_eq!(
            result,
            Err(RunnerError::EcOpSameXCoordinate(
                EcOpBuiltinRunner::format_ec_op_error(
                    (felt_to_bigint(partial_sum.0), felt_to_bigint(partial_sum.1)),
                    felt_to_bigint(m),
                    (
                        felt_to_bigint(doubled_point.0),
                        felt_to_bigint(doubled_point.1)
                    )
                )
                .into_boxed_str()
            ))
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    /* Data taken from this program execution:
       %builtins output ec_op
       from starkware.cairo.common.cairo_builtins import EcOpBuiltin
       from starkware.cairo.common.serialize import serialize_word
       from starkware.cairo.common.ec_point import EcPoint
       from starkware.cairo.common.ec import ec_op

       func main{output_ptr: felt*, ec_op_ptr: EcOpBuiltin*}():
           let x: EcPoint = EcPoint(2089986280348253421170679821480865132823066470938446095505822317253594081284, 1713931329540660377023406109199410414810705867260802078187082345529207694986)

           let y: EcPoint = EcPoint(874739451078007766457464989774322083649278607533249481151382481072868806602,152666792071518830868575557812948353041420400780739481342941381225525861407)
           let z: EcPoint = ec_op(x,34, y)
           serialize_word(z.x)
           return()
           end
    */
    fn deduce_memory_cell_ec_op_for_preset_memory_valid() {
        let memory = memory![
            (
                (3, 0),
                (
                    "0x68caa9509b7c2e90b4d92661cbf7c465471c1e8598c5f989691eef6653e0f38",
                    16
                )
            ),
            (
                (3, 1),
                (
                    "0x79a8673f498531002fc549e06ff2010ffc0c191cceb7da5532acb95cdcb591",
                    16
                )
            ),
            (
                (3, 2),
                (
                    "0x1ef15c18599971b7beced415a40f0c7deacfd9b0d1819e03d723d8bc943cfca",
                    16
                )
            ),
            (
                (3, 3),
                (
                    "0x5668060aa49730b7be4801df46ec62de53ecd11abe43a32873000c36e8dc1f",
                    16
                )
            ),
            ((3, 4), 34),
            (
                (3, 5),
                (
                    "2778063437308421278851140253538604815869848682781135193774472480292420096757",
                    16
                )
            )
        ];
        let builtin = EcOpBuiltinRunner::new(&EcOpInstanceDef::default(), true);

        let result = builtin.deduce_memory_cell(Relocatable::from((3, 6)), &memory);
        assert_eq!(
            result,
            Ok(Some(MaybeRelocatable::from(felt_hex!(
                "3598390311618116577316045819420613574162151407434885460365915347732568210029"
            ))))
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn deduce_memory_cell_ec_op_for_preset_memory_unfilled_input_cells() {
        let memory = memory![
            (
                (3, 1),
                (
                    "0x79a8673f498531002fc549e06ff2010ffc0c191cceb7da5532acb95cdcb591",
                    10
                )
            ),
            (
                (3, 2),
                (
                    "0x1ef15c18599971b7beced415a40f0c7deacfd9b0d1819e03d723d8bc943cfca",
                    10
                )
            ),
            (
                (3, 3),
                (
                    "0x5668060aa49730b7be4801df46ec62de53ecd11abe43a32873000c36e8dc1f",
                    10
                )
            ),
            ((3, 4), 34),
            (
                (3, 5),
                (
                    "2778063437308421278851140253538604815869848682781135193774472480292420096757",
                    10
                )
            )
        ];

        let builtin = EcOpBuiltinRunner::new(&EcOpInstanceDef::default(), true);
        let result = builtin.deduce_memory_cell(Relocatable::from((3, 6)), &memory);
        assert_eq!(result, Ok(None));
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn deduce_memory_cell_ec_op_for_preset_memory_addr_not_an_output_cell() {
        let memory = memory![
            (
                (3, 0),
                (
                    "0x68caa9509b7c2e90b4d92661cbf7c465471c1e8598c5f989691eef6653e0f38",
                    10
                )
            ),
            (
                (3, 1),
                (
                    "0x79a8673f498531002fc549e06ff2010ffc0c191cceb7da5532acb95cdcb591",
                    10
                )
            ),
            (
                (3, 2),
                (
                    "0x1ef15c18599971b7beced415a40f0c7deacfd9b0d1819e03d723d8bc943cfca",
                    10
                )
            ),
            (
                (3, 3),
                (
                    "0x5668060aa49730b7be4801df46ec62de53ecd11abe43a32873000c36e8dc1f",
                    10
                )
            ),
            ((3, 4), 34),
            (
                (3, 5),
                (
                    "2778063437308421278851140253538604815869848682781135193774472480292420096757",
                    10
                )
            )
        ];
        let builtin = EcOpBuiltinRunner::new(&EcOpInstanceDef::default(), true);

        let result = builtin.deduce_memory_cell(Relocatable::from((3, 3)), &memory);
        assert_eq!(result, Ok(None));
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn deduce_memory_cell_ec_op_for_preset_memory_non_integer_input() {
        let memory = memory![
            (
                (3, 0),
                (
                    "0x68caa9509b7c2e90b4d92661cbf7c465471c1e8598c5f989691eef6653e0f38",
                    10
                )
            ),
            (
                (3, 1),
                (
                    "0x79a8673f498531002fc549e06ff2010ffc0c191cceb7da5532acb95cdcb591",
                    10
                )
            ),
            (
                (3, 2),
                (
                    "0x1ef15c18599971b7beced415a40f0c7deacfd9b0d1819e03d723d8bc943cfca",
                    10
                )
            ),
            ((3, 3), (1, 2)),
            ((3, 4), 34),
            (
                (3, 5),
                (
                    "2778063437308421278851140253538604815869848682781135193774472480292420096757",
                    10
                )
            )
        ];
        let builtin = EcOpBuiltinRunner::new(&EcOpInstanceDef::default(), true);

        assert_eq!(
            builtin.deduce_memory_cell(Relocatable::from((3, 6)), &memory),
            Err(RunnerError::Memory(MemoryError::ExpectedInteger(Box::new(
                Relocatable::from((3, 3))
            ))))
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn get_memory_segment_addresses() {
        let builtin = EcOpBuiltinRunner::new(&EcOpInstanceDef::default(), true);

        assert_eq!(builtin.get_memory_segment_addresses(), (0, None));
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn get_memory_accesses_missing_segment_used_sizes() {
        let builtin =
            BuiltinRunner::EcOp(EcOpBuiltinRunner::new(&EcOpInstanceDef::default(), true));
        let vm = vm!();

        assert_eq!(
            builtin.get_memory_accesses(&vm),
            Err(MemoryError::MissingSegmentUsedSizes),
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn get_memory_accesses_empty() {
        let builtin =
            BuiltinRunner::EcOp(EcOpBuiltinRunner::new(&EcOpInstanceDef::default(), true));
        let mut vm = vm!();

        vm.segments.segment_used_sizes = Some(vec![0]);
        assert_eq!(builtin.get_memory_accesses(&vm), Ok(vec![]));
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn get_memory_accesses() {
        let builtin =
            BuiltinRunner::EcOp(EcOpBuiltinRunner::new(&EcOpInstanceDef::default(), true));
        let mut vm = vm!();

        vm.segments.segment_used_sizes = Some(vec![4]);
        assert_eq!(
            builtin.get_memory_accesses(&vm),
            Ok(vec![
                (builtin.base() as isize, 0).into(),
                (builtin.base() as isize, 1).into(),
                (builtin.base() as isize, 2).into(),
                (builtin.base() as isize, 3).into(),
            ]),
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn get_used_cells_missing_segment_used_sizes() {
        let builtin =
            BuiltinRunner::EcOp(EcOpBuiltinRunner::new(&EcOpInstanceDef::default(), true));
        let vm = vm!();

        assert_eq!(
            builtin.get_used_cells(&vm.segments),
            Err(MemoryError::MissingSegmentUsedSizes)
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn get_used_cells_empty() {
        let builtin =
            BuiltinRunner::EcOp(EcOpBuiltinRunner::new(&EcOpInstanceDef::default(), true));
        let mut vm = vm!();

        vm.segments.segment_used_sizes = Some(vec![0]);
        assert_eq!(builtin.get_used_cells(&vm.segments), Ok(0));
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn get_used_cells() {
        let builtin =
            BuiltinRunner::EcOp(EcOpBuiltinRunner::new(&EcOpInstanceDef::default(), true));
        let mut vm = vm!();

        vm.segments.segment_used_sizes = Some(vec![4]);
        assert_eq!(builtin.get_used_cells(&vm.segments), Ok(4));
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn initial_stackincluded_test() {
        let ec_op_builtin: BuiltinRunner =
            EcOpBuiltinRunner::new(&EcOpInstanceDef::default(), true).into();
        assert_eq!(ec_op_builtin.initial_stack(), vec![mayberelocatable!(0, 0)])
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn initial_stack_notincluded_test() {
        let ec_op_builtin = EcOpBuiltinRunner::new(&EcOpInstanceDef::default(), false);
        assert_eq!(ec_op_builtin.initial_stack(), Vec::new())
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn catch_point_same_x() {
        let program =
            include_bytes!("../../../../../cairo_programs/bad_programs/ec_op_same_x.json");
        let cairo_run_config = crate::cairo_run::CairoRunConfig {
            layout: "all_cairo",
            ..crate::cairo_run::CairoRunConfig::default()
        };
        let result = crate::cairo_run::cairo_run(
            program,
            &cairo_run_config,
            &mut BuiltinHintProcessor::new_empty(),
        );
        assert!(result.is_err());
        // We need to check this way because CairoRunError doens't implement PartialEq
        match result {
            Err(CairoRunError::VirtualMachine(VirtualMachineError::RunnerError(
                RunnerError::EcOpSameXCoordinate(_),
            ))) => {}
            Err(_) => panic!("Wrong error returned, expected RunnerError::EcOpSameXCoordinate"),
            Ok(_) => panic!("Expected run to fail"),
        }
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn catch_point_not_in_curve() {
        let program =
            include_bytes!("../../../../../cairo_programs/bad_programs/ec_op_not_in_curve.json");
        let cairo_run_config = crate::cairo_run::CairoRunConfig {
            layout: "all_cairo",
            ..crate::cairo_run::CairoRunConfig::default()
        };
        let result = crate::cairo_run::cairo_run(
            program,
            &cairo_run_config,
            &mut BuiltinHintProcessor::new_empty(),
        );
        assert!(result.is_err());

        // We need to check this way because CairoRunError doens't implement PartialEq
        match result {
            Err(CairoRunError::VirtualMachine(VirtualMachineError::RunnerError(
                RunnerError::PointNotOnCurve(_),
            ))) => {}
            Err(_) => panic!("Wrong error returned, expected RunnerError::EcOpSameXCoordinate"),
            Ok(_) => panic!("Expected run to fail"),
        }
    }
}