use crate::address_space::MemoryReader;
use crate::arch::{Architecture, Registers, UnwindFailure, UnwindMode, UnwindStatus};
use std::marker::PhantomData;

pub struct UnwindContext<A: Architecture> {
    nth_frame: usize,
    initial_address: Option<A::RegTy>,
    ra_address: Option<A::RegTy>,
    address: A::RegTy,
    regs: A::Regs,
    state: A::State,
    mode: UnwindMode,
    is_done: bool,
    last_failure: Option<UnwindFailure>,
    panic_on_partial_backtrace: bool,

    phantom: PhantomData<A>,
}

pub struct UnwindHandle<'a, A: Architecture + 'a> {
    ctx: &'a mut UnwindContext<A>,
}

// We define this trait to be able to put the `#[inline(always)]`
// on the register fetching callback to guarantee that we won't
// produce any extra frames when unwinding locally.
pub trait InitializeRegs<A: Architecture> {
    fn initialize_regs(self, regs: &mut A::Regs);
}

impl<T, A: Architecture> InitializeRegs<A> for T
where
    T: FnOnce(&mut A::Regs),
{
    #[inline(always)]
    fn initialize_regs(self, regs: &mut A::Regs) {
        self(regs)
    }
}

impl<A: Architecture> UnwindContext<A> {
    pub fn new() -> Self {
        UnwindContext {
            nth_frame: 0,
            initial_address: None,
            ra_address: None,
            address: Default::default(),
            regs: Default::default(),
            state: A::initial_state(),
            mode: UnwindMode::Default,
            panic_on_partial_backtrace: false,
            is_done: true,
            last_failure: None,
            phantom: PhantomData,
        }
    }

    pub(crate) fn set_panic_on_partial_backtrace(&mut self, value: bool) {
        self.panic_on_partial_backtrace = value;
    }

    #[inline(always)]
    pub(crate) fn start_with_mode<'a, M: MemoryReader<A>, T: InitializeRegs<A>>(
        &'a mut self,
        memory: &M,
        mode: UnwindMode,
        initializer: T,
    ) -> UnwindHandle<'a, A> {
        self.mode = mode;
        initializer.initialize_regs(&mut self.regs);
        self.start_impl(memory)
    }

    pub(crate) fn clear_cache(&mut self) {
        A::clear_cache(&mut self.state);
    }

    pub(crate) fn last_failure(&self) -> Option<UnwindFailure> {
        self.last_failure
    }

    fn start_impl<'a, M: MemoryReader<A>>(&'a mut self, memory: &M) -> UnwindHandle<'a, A> {
        self.is_done = false;
        self.nth_frame = 0;
        self.last_failure = None;

        self.address = self.regs.get(A::INSTRUCTION_POINTER_REG).unwrap();
        let previous_address = self.address.into();
        let previous_stack_pointer = self
            .regs
            .get(A::STACK_POINTER_REG)
            .map(|value| value.into());
        debug!("Starting unwinding at: 0x{:016X}", self.address);

        let result = A::unwind_with_mode(
            0,
            memory,
            &mut self.state,
            &mut self.regs,
            &mut self.initial_address,
            &mut self.ra_address,
            self.mode,
        );
        match result {
            Err(error) => {
                if self.panic_on_partial_backtrace {
                    panic!("Partial backtrace!");
                }

                self.last_failure = Some(error);
                self.is_done = true;
            }
            Ok(UnwindStatus::Finished) => self.is_done = true,
            Ok(UnwindStatus::InProgress) => {
                if self.did_not_make_progress(previous_address, previous_stack_pointer) {
                    self.last_failure = Some(UnwindFailure::MissingReturnAddress);
                    self.is_done = true;
                }
            }
        };

        UnwindHandle { ctx: self }
    }

    fn did_not_make_progress(
        &self,
        previous_address: u64,
        previous_stack_pointer: Option<u64>,
    ) -> bool {
        let next_address = self
            .regs
            .get(A::INSTRUCTION_POINTER_REG)
            .map(|value| value.into());
        let next_stack_pointer = self
            .regs
            .get(A::STACK_POINTER_REG)
            .map(|value| value.into());
        let no_progress =
            next_address == Some(previous_address) && next_stack_pointer == previous_stack_pointer;
        if no_progress {
            warn!(
                "unwind made no progress at 0x{:016X}; stopping before emitting a repeated stack",
                previous_address
            );
        }
        no_progress
    }
}

impl<'a, A: Architecture> UnwindHandle<'a, A> {
    pub fn unwind<M: MemoryReader<A>>(&mut self, memory: &M) -> bool {
        if self.ctx.is_done {
            return false;
        }

        self.ctx.nth_frame += 1;

        if self.ctx.nth_frame > 1000 {
            warn!("possible infinite loop detected and avoided");
            return false;
        }

        self.ctx.address = self.ctx.regs.get(A::INSTRUCTION_POINTER_REG).unwrap();
        let previous_address = self.ctx.address.into();
        let previous_stack_pointer = self
            .ctx
            .regs
            .get(A::STACK_POINTER_REG)
            .map(|value| value.into());
        debug!(
            "Unwinding #{} -> #{} at: 0x{:016X}",
            self.ctx.nth_frame - 1,
            self.ctx.nth_frame,
            self.ctx.address
        );

        self.ctx.initial_address = None;
        self.ctx.ra_address = None;
        let result = A::unwind_with_mode(
            self.ctx.nth_frame,
            memory,
            &mut self.ctx.state,
            &mut self.ctx.regs,
            &mut self.ctx.initial_address,
            &mut self.ctx.ra_address,
            self.ctx.mode,
        );
        match result {
            Err(error) => {
                if self.ctx.panic_on_partial_backtrace {
                    panic!("Partial backtrace!");
                }

                self.ctx.last_failure = Some(error);
                self.ctx.is_done = true;
            }
            Ok(UnwindStatus::Finished) => self.ctx.is_done = true,
            Ok(UnwindStatus::InProgress) => {
                if self
                    .ctx
                    .did_not_make_progress(previous_address, previous_stack_pointer)
                {
                    self.ctx.last_failure = Some(UnwindFailure::MissingReturnAddress);
                    self.ctx.is_done = true;
                    return true;
                }
                debug!(
                    "Current address on frame #{}: 0x{:016X}",
                    self.ctx.nth_frame, self.ctx.address
                );
            }
        };

        true
    }

    #[inline]
    pub fn current_initial_address(&mut self) -> Option<A::RegTy> {
        self.ctx.initial_address
    }

    #[inline]
    pub fn current_address(&self) -> A::RegTy {
        self.ctx.address
    }

    #[cfg(feature = "local-unwinding")]
    #[inline]
    pub fn next_address_location(&mut self) -> Option<A::RegTy> {
        self.ctx.ra_address
    }

    #[cfg(feature = "local-unwinding")]
    #[inline]
    pub fn next_stack_pointer(&self) -> A::RegTy {
        self.ctx.regs.get(A::STACK_POINTER_REG).unwrap()
    }

    #[cfg(feature = "local-unwinding")]
    #[inline]
    pub fn replace_next_address(&mut self, value: A::RegTy) {
        self.ctx.regs.append(A::INSTRUCTION_POINTER_REG, value)
    }
}
