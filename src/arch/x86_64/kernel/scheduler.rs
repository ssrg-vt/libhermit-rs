// Copyright (c) 2017 Stefan Lankes, RWTH Aachen University
//               2018 Colin Finck, RWTH Aachen University
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

//! Architecture dependent interface to initialize a task

use alloc::rc::Rc;
use arch::x86_64::kernel::apic;
use arch::x86_64::kernel::gdt;
use arch::x86_64::kernel::idt;
use arch::x86_64::kernel::irq;
use arch::x86_64::kernel::percore::*;
use arch::x86_64::kernel::processor;
use arch::x86_64::kernel::copy_safe::*;
use config::*;
use core::cell::RefCell;
use core::mem;
use core::ptr::write_bytes;
use environment;
use mm;
use scheduler::task::{Task, TaskFrame, TaskTLS};

#[repr(C, packed)]
struct State {
    /// PKRU
    pkru: usize,
	/// FS register for TLS support
	fs: usize,
	/// R15 register
	r15: usize,
	/// R14 register
	r14: usize,
	/// R13 register
	r13: usize,
	/// R12 register
	r12: usize,
	/// R11 register
	r11: usize,
	/// R10 register
	r10: usize,
	/// R9 register
	r9: usize,
	/// R8 register
	r8: usize,
	/// RDI register
	rdi: usize,
	/// RSI register
	rsi: usize,
	/// RBP register
	rbp: usize,
	/// RBX register
	rbx: usize,
	/// RDX register
	rdx: usize,
	/// RCX register
	rcx: usize,
	/// RAX register
	rax: usize,
	/// status flags
	rflags: usize,
	/// instruction pointer
	rip: usize,
}

#[derive(Default)]
pub struct TaskStacks {
	/// Whether this is a boot stack
	is_boot_stack: bool,
	/// Stack of the task
	pub stack: usize,
	/// Stack to handle asynchronous interrupts
	pub ist0: usize,
	/// Isolated stack of the task
	pub isolated_stack: usize,
	/// User stack
	pub user_stack: usize,

	//pub current_kernel_stack: usize,
	//pub current_user_stack: usize,
}

impl TaskStacks {
	pub fn new() -> Self {
		// Allocate an executable stack to possibly support dynamically generated code on the stack (see https://security.stackexchange.com/a/47825).
		let stack = ::mm::allocate(DEFAULT_STACK_SIZE, true);
		//info!("Allocating stack {:#X} ~ {:#X}", stack, stack + DEFAULT_STACK_SIZE);

		let ist0 = ::mm::user_allocate(KERNEL_STACK_SIZE, true);
		//info!("Allocating stack {:#X} ~ {:#X}", stack, stack + KERNEL_STACK_SIZE);

		let isolated_stack = ::mm::unsafe_allocate(DEFAULT_STACK_SIZE, true);
		//info!("Allocating isolated_stack {:#X} ~ {:#X}", isolated_stack, isolated_stack + DEFAULT_STACK_SIZE);

		let user_stack = ::mm::user_allocate(DEFAULT_STACK_SIZE, true);
		//info!("Allocating user_stack {:#X} ~ {:#X}", user_stack, user_stack + DEFAULT_STACK_SIZE);

		Self {
			is_boot_stack: false,
			stack: stack,
			ist0: ist0,
			isolated_stack: isolated_stack,
			user_stack: user_stack,
			//current_kernel_stack: 0xaaaabeefusize,
			//current_user_stack: user_stack + DEFAULT_STACK_SIZE,
		}
	}

	pub fn from_boot_stacks() -> Self {
		let tss = unsafe { &(*PERCORE.tss.get()) };
		let stack = tss.rsp[0] as usize + 0x10 - KERNEL_STACK_SIZE;
		debug!("Using boot stack {:#X}", stack);
		let ist0 = tss.ist[0] as usize + 0x10 - KERNEL_STACK_SIZE;
		debug!("IST0 is located at {:#X}", ist0);

		Self {
			is_boot_stack: true,
			stack: stack,
			ist0: ist0,
			isolated_stack: 0usize,
			user_stack: 0usize,
			//current_kernel_stack: 0xeeeebeefusize,
			//current_user_stack: 0xffffbeefusize,
		}
	}
}

impl Drop for TaskStacks {
	fn drop(&mut self) {
		if !self.is_boot_stack {
			debug!("Deallocating stack {:#X} and ist0 {:#X}", self.stack, self.ist0);

			::mm::deallocate(self.stack, DEFAULT_STACK_SIZE);
			::mm::deallocate(self.ist0, KERNEL_STACK_SIZE);

			debug!("Deallocating isolated_stack {:#X}", self.stack);

			::mm::deallocate(self.isolated_stack, DEFAULT_STACK_SIZE);

			::mm::deallocate(self.user_stack, DEFAULT_STACK_SIZE);
		}
	}
}

extern "C" fn leave_task() -> ! {
	core_scheduler().exit(0);
}

#[cfg(test)]
extern "C" fn task_entry(func: extern "C" fn(usize), arg: usize) {}

#[cfg(not(test))]
extern "C" fn task_entry(func: extern "C" fn(usize), arg: usize) {
	// determine the size of tdata (tls without tbss)
	let tdata_size: usize = environment::get_tls_filesz();

	// Check if the task (process or thread) uses Thread-Local-Storage.
	let tls_size = environment::get_tls_memsz();
	if tls_size > 0 {
		// Yes, it does, so we have to allocate TLS memory.
		// Allocate enough space for the given size and one more variable of type usize, which holds the tls_pointer.
		let tls_allocation_size = align_up!(tls_size, 32) + mem::size_of::<usize>();
		let tls = TaskTLS::new(tls_allocation_size);

		// The tls_pointer is the address to the end of the TLS area requested by the task.
		let tls_pointer = tls.address() + align_up!(tls_size, 32);
		unsafe {
			// The x86-64 TLS specification also requires that the tls_pointer can be accessed at fs:0.
			// This allows TLS variable values to be accessed by "mov rax, fs:0" and a later "lea rdx, [rax+VARIABLE_OFFSET]".
			// See "ELF Handling For Thread-Local Storage", version 0.20 by Ulrich Drepper, page 12 for details.
			//
			// fs:0 is where tls_pointer points to and we have reserved space for a usize value above.
			isolation_start!();
			*(tls_pointer as *mut usize) = tls_pointer;
			isolation_end!();
		}
		// As per the x86-64 TLS specification, the FS register holds the tls_pointer.
		// This allows TLS variable values to be accessed by "mov rax, fs:VARIABLE_OFFSET".
		processor::writefs(tls_pointer);
		debug!(
			"Set FS to 0x{:x}, TLS size 0x{:x}, TLS data size 0x{:x}",
			tls_pointer, tls_size, tdata_size
		);
		/* Copy TLS variables with their initial values on the tls's unsafe_storage.
			Then copy back the TLS variables with their initial values on tls.address()
		*/
		list_add(environment::get_tls_start());
		list_add(tls.address());
		copy_from_safe(environment::get_tls_start() as *const u8, tdata_size);
		copy_to_safe(tls.address() as *mut u8, tls_size);
		clear_unsafe_storage();

		// Associate the TLS memory to the current task.
		let mut current_task_borrowed = core_scheduler().current_task.borrow_mut();
		debug!(
			"Set up TLS for task {} at address {:#X}",
			current_task_borrowed.id,
			align_up!(tls.address(), 32)
		);
		current_task_borrowed.tls = Some(Rc::new(RefCell::new(tls)));
	}

	//info!("Task_entry of {}", core_scheduler().current_task.borrow().id.into());
	if core_scheduler().current_task.borrow().id.into() >= 2 {
		user_start!(false);
		func(arg);
		user_end!();
	} else {
		// Call the actual entry point of the task.
		func(arg);
	}
}

impl TaskFrame for Task {
	fn create_stack_frame(&mut self, func: extern "C" fn(usize), arg: usize) {
		/* This function initializes an empty stack frame.
		   So we can just set pages to SHARE_MEM_REGION then set it back to SAFE_MEM_RGION after the initializtion.
		*/
		use arch::x86_64::mm::paging::{set_pkey_on_page_table_entry, BasePageSize};
		set_pkey_on_page_table_entry::<BasePageSize>(self.stacks.stack, DEFAULT_STACK_SIZE/4096, mm::SHARED_MEM_REGION);
		unsafe {
			// Mark the entire stack with 0xCD.
			let temp_stack = self.stacks.stack;
			isolate_function_weak!(write_bytes(temp_stack as *mut u8, 0xCD, DEFAULT_STACK_SIZE));

			// Set a marker for debugging at the very top.
			let mut stack = (self.stacks.stack + DEFAULT_STACK_SIZE - 0x10) as *mut usize;
			isolation_start!();
			*stack = 0xDEAD_BEEFusize;
			isolation_end!();

			// Put the leave_task function on the stack.
			// When the task has finished, it will call this function by returning.
			stack = (stack as usize - mem::size_of::<usize>()) as *mut usize;
			isolation_start!();
			*stack = leave_task as usize;
			isolation_end!();
			// Put the State structure expected by the ASM switch() function on the stack.
			stack = (stack as usize - mem::size_of::<State>()) as *mut usize;

			let state = stack as *mut State;
			isolation_start!();
			let state_ref = &mut *state;
			isolation_end!();
			isolate_function_weak!(write_bytes(state as *mut u8, 0, mem::size_of::<State>()));

			(*state_ref).rip = task_entry as usize;
			(*state_ref).rdi = func as usize;
			(*state_ref).rsi = arg as usize;
			(*state_ref).rflags = 0x1202usize;

			// Set the task's stack pointer entry to the stack we have just crafted.
			self.last_stack_pointer = stack as usize;
			self.user_stack_pointer = self.stacks.user_stack as usize + DEFAULT_STACK_SIZE;
		}
		set_pkey_on_page_table_entry::<BasePageSize>(self.stacks.stack, DEFAULT_STACK_SIZE/4096, mm::SAFE_MEM_REGION);
	}
}

extern "x86-interrupt" fn timer_handler(_stack_frame: &mut irq::ExceptionStackFrame) {
	core_scheduler().blocked_tasks.lock().handle_waiting_tasks();
	apic::eoi();
	core_scheduler().scheduler();
}

pub fn install_timer_handler() {
	idt::set_gate(apic::TIMER_INTERRUPT_NUMBER, timer_handler as usize, 0);
}
