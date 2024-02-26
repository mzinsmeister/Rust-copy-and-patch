
pub mod stencils;
pub mod ir;
mod disassemble;
mod copy_patch;
mod expr_codegen;


use std::{cell::RefCell, hint::black_box, ops::Deref, ptr};
use libc::c_void;
use stencils::Stencil;

use crate::codegen::{copy_patch::STENCILS, ir::DataType};

use self::{copy_patch::CopyPatchBackend, ir::ConstValue};

pub use expr_codegen::{get_type, generate_code, CodeGenError};



// This is a simple example of how we can generate code that calls back into pre-compiled
// functions using copy-and-patch
#[allow(dead_code)]
unsafe extern "C" fn hello_world(args: *mut u8) -> *mut u8 {
    let u64_ptr = args as *mut u64;
    let arg1 = u64_ptr.read_unaligned();
    println!("Hello, world from the generated Code! {}", arg1);
    42 as *mut u8
}

pub(crate) fn init_stencils() {
    // Dummy access to initialize stencils
    let compile_start = std::time::Instant::now();
    black_box(STENCILS.len());
    let compile_elapsed = compile_start.elapsed();
    println!("Stencil initialization: {:?}", compile_elapsed);
}

// TODO: Once we go beyond basic arithmetic expressions we should have our own IR
//       We should also have a way to represent/address values so that we can insert
//       put/take instructions automatically and so that we can also map the same logic to LLVM IR

pub struct GeneratedCode {
    pub stack: *mut u8,
    pub code: *const c_void,
    pub code_len: usize,
    pub ghcc_code: *const c_void,
}

impl GeneratedCode {

    pub fn new(stack_size: usize, wrapper_stencil: &Stencil, code: &[u8]) -> Self {

        // mmap a memory region with read and execute permissions
        let mmap = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                code.len(),
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };

        let mut ghcc_code = wrapper_stencil.code.clone();

        let holes_values = vec![(mmap as u64).to_ne_bytes()];
        for (&ofs, val) in wrapper_stencil.holes.iter().zip(holes_values.iter()) {
            ghcc_code[ofs..ofs + 8].copy_from_slice(val);
        }

        let ghcc_fun = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                ghcc_code.len(),
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };

        // Allocate stack space for our generated code
        // TODO: We could (and maybe should) also use the actual stack for this
        let stack_space = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                stack_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            ) as *mut u8
        };
    
        unsafe {
            std::ptr::copy_nonoverlapping(ghcc_code.as_ptr(), ghcc_fun as *mut u8, ghcc_code.len());
        }
    
        // copy the code to the memory region
        unsafe {
            std::ptr::copy_nonoverlapping(code.as_ptr(), mmap as *mut u8, code.len());
        }

        Self {
            stack: stack_space,
            code: mmap,
            code_len: code.len(),
            ghcc_code: ghcc_fun,
        }
    }
    
    pub fn call<T: Sized>(&self, args: &[i64]) -> T {
            // cast the memory region to a function pointer
        let f: extern "C" fn(*mut u8) = unsafe { std::mem::transmute(self.ghcc_code) };

        // Copy args to the stack
        for (i, item) in args.iter().enumerate() {
            unsafe {
                std::ptr::write_unaligned((self.stack as *mut i64).offset(i as isize), *item);
            }
        }

        // call the function with the stack pointer in RAX
        // This should work, however it requires inline asm and it requires us to use 
        // a "root stencil" that unpacks all of our arguments from our custom stack into
        // the registers that the other stencils expect but that is only done once per 
        // call so it should be fine
        f(self.stack);
        // get the result from the stack;

        unsafe {
            let value = std::ptr::read_unaligned(self.stack as *const u64);
            std::mem::transmute_copy(&value)
        }
    }
}

impl Drop for GeneratedCode {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.code as *mut libc::c_void, 0x1000);
            libc::munmap(self.ghcc_code as *mut libc::c_void, 0x1000);
            libc::munmap(self.stack as *mut libc::c_void, 0x1000);
        }
    }
}

fn get_fn_ptr(f: unsafe extern "C" fn(*mut u8) -> *mut u8) -> *const c_void {
    f as *const c_void
}


// We try to write a nice rustic API here that is easy to use
// and takes advantage of the borrow checker to make sure that
// we don't do stupid shit. Therefore we will introduce wrappers
// representing values here which will then allow us to do all the
// register/stack moving automatically while still beeing efficient
// and reusing values in registers as much as possible.

#[derive(Debug, Clone)]
enum CGValue {
    Variable{
        data_type: DataType, 
        stack_pos: usize,
        readonly: bool
    },
    Constant(ConstValue),
    Free(Option<usize>)
}

struct CGValueRef<'cg> {
    i: usize,
    cg: &'cg CodeGen,
    data_type: DataType
}

impl<'cg> Drop for CGValueRef<'cg> {
    fn drop(&mut self) {
        self.cg.free_value(self);
    }
}

impl<'cg> CGValueRef<'cg> {
    fn new(i: usize, cg: &'cg CodeGen, data_type: DataType) -> Self {
        CGValueRef { i, cg, data_type }
    }

    fn new_readonly(i: usize, cg: &'cg CodeGen, data_type: DataType) -> Self {
        CGValueRef { i, cg, data_type }
    }
}

impl<'cg> PartialEq for CGValueRef<'cg> {
    fn eq(&self, other: &Self) -> bool {
        self.i == other.i && ptr::eq(self.cg, other.cg)
    }
}

impl<'cg> Eq for CGValueRef<'cg,> {}

impl<'cg> PartialOrd for CGValueRef<'cg> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        if !ptr::eq(self.cg, other.cg) {
            None
        } else {
            self.i.partial_cmp(&other.i)
        }
    }
}

impl<'cg> std::fmt::Debug for CGValueRef<'cg> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CGValueRef({}, {:x})", self.i, ptr::addr_of!(self.cg) as usize)
    }
}

#[derive(Debug, PartialEq, PartialOrd, Eq)]
struct I64Ref<'cg> (CGValueRef<'cg>);

impl<'cg> Into<CGValueRef<'cg>> for I64Ref<'cg> {
    fn into(self) -> CGValueRef<'cg> {
        self.0
    }
}

impl Clone for I64Ref<'_> {
    fn clone(&self) -> Self {
        let cg = self.0.cg;
        let clone = I64Ref(cg.clone_value(&self.0));
        clone
    }
}

impl<'cg> From<CGValueRef<'cg>> for I64Ref<'cg> {
    fn from(v: CGValueRef<'cg>) -> Self {
        I64Ref(v)
    }
}

impl<'cg> std::ops::Add for &I64Ref<'cg> {
    type Output = I64Ref<'cg>;

    fn add(self, rhs: Self) -> Self::Output {
        let cg = self.0.cg;
        let clone = I64Ref(cg.clone_value(&self.0));
        clone + rhs
    }
}

// this actually takes ownership of the value which means we can overwrite it
impl<'cg> std::ops::Add<&Self> for I64Ref<'cg> {
    type Output = I64Ref<'cg>;

    fn add(self, rhs: &Self) -> Self::Output {
        let cg = self.0.cg;
        let cgvref = cg.add(&self.0, &rhs.0);
        if cgvref == self.0.i {
            self
        } else {
            I64Ref(CGValueRef::new(cgvref, cg, self.0.data_type.clone()))
        }
    }
}

impl<'cg> std::ops::AddAssign<&Self> for I64Ref<'cg> {
    fn add_assign(&mut self, rhs: &Self) {
        let cg = self.0.cg;
        let cgvref = cg.add(&self.0, &rhs.0);
        if cgvref != self.0.i {
            self.0 = CGValueRef::new(cgvref, cg, self.0.data_type.clone());
        }
    }
}


#[derive(Debug, PartialEq, PartialOrd, Eq)]
struct BoolRef<'cg> (CGValueRef<'cg>);

impl<'cg> Into<CGValueRef<'cg>> for BoolRef<'cg> {
    fn into(self) -> CGValueRef<'cg> {
        self.0
    }
}

impl<'cg> From<CGValueRef<'cg>> for BoolRef<'cg> {
    fn from(v: CGValueRef<'cg>) -> Self {
        BoolRef(v)
    }
}

impl Clone for BoolRef<'_> {
    fn clone(&self) -> Self {
        let cg = self.0.cg;
        let clone = BoolRef(cg.clone_value(&self.0));
        clone
    }
}

struct CodeGen {
    inner: RefCell<CodeGenInner>,
}

impl CodeGen {

    fn new(args: usize) -> Self {
        let cg = Self {
            inner: RefCell::new(CodeGenInner::new(args)),
        };
        cg        
    }

    // We make sure arguments are immutable so having multiple references to them is not a problem
    pub fn get_arg(&self, n: usize) -> I64Ref {
        I64Ref(CGValueRef::new_readonly(n, self, DataType::I64))
    }

    pub fn new_i64_const(&self, n: i64) -> I64Ref {
        let inner = &mut self.inner.borrow_mut();
        let i = inner.values.len();
        inner.values.push(CGValue::Constant(ConstValue::I64(n)));
        I64Ref(CGValueRef::new(i, self, DataType::I64))
    }

    pub fn new_bool_const(&self, b: bool) -> BoolRef {
        let inner = &mut self.inner.borrow_mut();
        let i = inner.values.len();
        inner.values.push(CGValue::Constant(ConstValue::Bool(b)));
        BoolRef(CGValueRef::new(i, self, DataType::Bool))
    }

    fn free_value(&self, v: &CGValueRef) {
        self.inner.borrow_mut().free_value(v.i);
    }

    fn clone_value(&self, v: &CGValueRef) -> CGValueRef {
        let i = self.inner.borrow_mut().clone_value(v.i);
        CGValueRef::new(i, self, v.data_type.clone())
    }

    fn add(&self, l: &CGValueRef, r: &CGValueRef) -> usize {
        let cg = &mut self.inner.borrow_mut();
        cg.add(l.i, r.i)
    }

    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.inner.borrow_mut().reset();
    }

    pub fn generate_return<'a, D: Deref<Target = CGValueRef<'a>>>(&self, return_value: D) {
        let cg = &mut self.inner.borrow_mut();
        cg.generate_return(return_value.i);
    }

    // Takes Ownership of the return value and resets all registers
    // TODO: References will be invalid after this. We cannot enforce 
    pub fn generate_code(&self) -> GeneratedCode {
        let cg = &mut self.inner.borrow_mut();
        cg.generate_code()
    }

}



// How this works is that we have "values" which are roughly equivalent to 
// variables in a high level language. Note that we don't do SSA here. I
// don't think it's really beneficial for Copy and patch as it would probably
// make it harder to detect whether we can reuse a value in a register or not
// However keep the invariant that all mutable values (which for now are all
// values except for the arguments) are only referenced exactly once. The borrow checker
// helps us with this. We also keep the state of our registers, most notably what values
// are currently in them and whether they are dirty or not. Even Constants can be in registers
// however once they become dirty we change their type to variable and allocate stack space for them.
// The same is true for readonly values. We also manage free slots (as soon as the reference to a value)
// is droppped, it is actually freed through RAII (Drop trait).
//
// Operations have to make sure to call the dirty_reg1/2 (only dirty_reg1 is implemented for now)
// function whenever they change the value in a register. This function will then make sure that
// the value is marked dirty and spilled to the stack if necessary. We load values into registers
// lazily. Just because you create a constant, that doesn't mean there will be any code generated
// for it. We only load it into a register when we actually need it. We also try to move between
// registers and stack as little as possible. 

/// Convenience layer around copy and patch compilation backend
/// so that you don't have to think about registers anymore
struct CodeGenInner {
    args_size: usize,
    values: Vec<CGValue>,
    free_slots: Vec<usize>,
    reg_state: [Option<(usize, bool)>; 2], // We save whether a register is potentially dirty
    inner: CopyPatchBackend,
    stack_ptr: usize, // TODO: Use actual byte sizes. For now we just use 8 bytes for everything
    stack_size: usize
}

impl CodeGenInner {

    fn new(args: usize) -> Self {
        let values = (0..args).into_iter()
            .map(|i| CGValue::Variable{ data_type: DataType::I64, stack_pos: i * 8, readonly: true})
            .collect();
        Self {
            args_size: args,
            values,
            free_slots: Vec::new(),
            reg_state: [None, None],
            inner: CopyPatchBackend::new(args),
            stack_ptr: args * 8,
            stack_size: args * 8,
        }
    }

    #[allow(dead_code)]
    fn reset(&mut self) {
        self.reg_state = [None, None];
        self.values.clear();
        self.free_slots.clear();
        self.stack_ptr = self.args_size * 8;
        self.stack_size = self.args_size * 8;
        self.inner.reset();
    }

    fn free_reg(&mut self, reg: usize, new_i: usize) -> bool {
        if let Some((i, dirty)) = &mut self.reg_state[reg] {
            if *dirty {
                let cur_value = &self.values[*i];
                match cur_value {
                    CGValue::Variable{readonly, stack_pos,..} => {
                        if *readonly {
                            panic!("We should have allocated a stack slot before dirtying a readonly value");
                        }
                        if *i != new_i {
                            // Save it to its designated stack location
                            match reg {
                                0 => self.inner.generate_put_stack(*stack_pos),
                                _ => todo!("Implement dirtying of second register"),
                            }
                        } else {
                            return true;
                        }
                    },
                    CGValue::Constant(_) => {
                        panic!("We should have allocated a stack slot before dirtying a const value");
                    },
                    CGValue::Free(_) => {},
                }
            }
        }
        false
    }

    fn put_in_reg1(&mut self, v: usize) {
        if self.free_reg(0, v) {
            return;
        }

        // Lookup which value is currently in the first register
        let reg2_state = self.reg_state[1].clone();
        if let Some((i, _)) = reg2_state {
            if i == v {
                todo!("Also support moving from second to first register")
            }
        }
        let value = &self.values[v];
        // Load the value into the first scratch register
        match value {
            CGValue::Variable{stack_pos,..}=> {
                // TODO: do take stack with datatype
                self.inner.generate_take_1_stack(*stack_pos);
            },
            CGValue::Constant(c) => {
                self.inner.generate_take_1_const(*c);
            },
            CGValue::Free(_) => unreachable!("We shouldn't even be able to have a reference to a free value"),
        }
        self.reg_state[0] = Some((v, false));
    }
    
    fn put_in_reg2(&mut self, v: usize) {
        self.free_reg(1, v);

        // Is our value in the first register? if so we can just swap the registers
        let reg1_state = self.reg_state[0].clone();
        if let Some((i, dirty)) = reg1_state {
            if i == v {
                if dirty {
                    todo!("Handle this case"); // should currently not happen
                }
                self.inner.generate_duplex();
                self.reg_state[1] = self.reg_state[0].clone();
                return;
            }
        }
        let value = &self.values[v];
        // Load the value into the first scratch register
        match value {
            CGValue::Variable{stack_pos, ..} => {
                // TODO: do take stack with datatype
                self.inner.generate_take_2_stack(*stack_pos);
            },
            CGValue::Constant(c) => {
                self.inner.generate_take_2_const(*c);
            },
            CGValue::Free(_) => unreachable!("We shouldn't even be able to have a reference to a free value"),
        }
        self.reg_state[1] = Some((v, false));
    }

    fn dirty_reg1(&mut self) -> Option<usize> {
        let reg_state = self.reg_state[0].clone();
        let reg_state2 = self.reg_state[1].clone();
        // If we have the same value in the second register, we must set it to free
        if let Some((i, _)) = reg_state2 { // TODO: What if its dirty here?
            if i == reg_state2.unwrap().0 {
                self.reg_state[1] = None;
            }
        }
        if let Some((i, _)) = reg_state {
            // Check whether the value is readonly and if it is we allocate a new value
            self.reg_state[0].as_mut().unwrap().1 = true;
            match &self.values[i] {
                CGValue::Variable{readonly,..} => {
                    if *readonly {
                        let slot = self.allocate_stack(DataType::I64);
                        self.reg_state[0].as_mut().unwrap().0 = slot;
                        return Some(slot);
                    } else {
                        return Some(i);
                    }
                },
                CGValue::Constant(c) => {
                    let stack_pos = self.stack_ptr;
                    self.stack_ptr += 8;
                    self.stack_size = self.stack_size.max(self.stack_ptr);
                    self.values[i] = CGValue::Variable { data_type: c.get_type(), stack_pos, readonly: false };
                    return Some(i);
                },
                _ => unreachable!(),
            }
        }
        None
    }

    #[allow(dead_code)]
    fn dirty_reg2(&mut self) -> Option<usize> {
        // This case is non-trivial because we also have to handle the case where value is already in reg1 and is dirty
        // In this case we could have two diverging states for the same value
        // Maybe this can't even happen as long as we keep the "one reference per mutable value" invariant
        todo!("Implement dirtying of second register")
    }

    fn allocate_stack(&mut self, data_type: DataType) -> usize {
        let i = if let Some(i) = self.free_slots.pop() {
            i
        } else {
            let i = self.values.len();
            self.values.push(CGValue::Free(None));
            i
        };
        let value = &mut self.values[i];
        match value {
            CGValue::Free(ref mut slot) => {
                let stack_ofs = if let Some(s) = slot {
                    *s
                } else {
                    let s = self.stack_ptr;
                    self.stack_ptr += 8;
                    self.stack_size = self.stack_size.max(self.stack_ptr);
                    s
                };
                *value = CGValue::Variable{ data_type, stack_pos: stack_ofs, readonly: false};
            },
            _ => unreachable!(),
        }
        i
    }

    /// Clones the value and returns it. Use the new value first if possible.
    fn clone_value(&mut self, v: usize) -> usize {
        let value = self.values[v].clone();
        match value {
            CGValue::Variable{data_type,..} => {
                if let Some((i, dirty)) = &mut self.reg_state[0] {
                    if *dirty {
                        self.inner.generate_put_stack(*i);
                        *dirty = false;
                    }
                    if *i == v {
                        *i = self.values.len();
                    }
                } else {
                    self.put_in_reg1(v);
                    self.reg_state[0].unwrap().0 = self.values.len();
                }
                self.allocate_stack(data_type)
            },
            CGValue::Constant(c) => {
                // We can just copy constants 
                // TODO: Do similar free slot handling for constants but only take Free(None)'s
                let i = self.values.len();
                self.values.push(CGValue::Constant(c));
                i
            },
            CGValue::Free(_) => unreachable!("We shouldn't even be able to have a reference to a free value"),
        }
    }

    fn free_value(&mut self, v: usize) {
        let value = &self.values[v];
        match value {
            CGValue::Variable{readonly, stack_pos,..} => {
                if *readonly {
                    return;
                }
                self.values[v] = CGValue::Free(Some(*stack_pos));
                self.free_slots.push(v);
            },
            CGValue::Constant(_) => {
                self.values[v] = CGValue::Free(None);
                self.free_slots.push(v);
            }
            CGValue::Free(_) => {/* TODO: Make sure double frees cannot happen, even internally */},
        }
    }

    // Move/Borrow semantics actually have a meaning here
    // If a value is moved it means that we can overwrite it
    // We specifically don't do SSA here.
    fn add(&mut self, l: usize, r: usize) -> usize {
        let vr = self.values[r].clone();
        self.put_in_reg1(l);
        match vr {
            CGValue::Variable{data_type,..} => {
                self.put_in_reg2(r);
                self.inner.generate_add(data_type);
            },
            CGValue::Constant(c) => {
                self.inner.generate_add_const(c);
            },
            CGValue::Free(_) => unreachable!("We shouldn't even be able to have a reference to a free value"),
        }
        self.dirty_reg1().unwrap()
    }

    fn generate_return(&mut self, return_value: usize) {
        self.put_in_reg1(return_value);
        self.inner.generate_put_stack(0);
        self.inner.generate_ret();
    }

    fn generate_code(&self) -> GeneratedCode {
        self.inner.generate_code(self.stack_size)
    }
}

