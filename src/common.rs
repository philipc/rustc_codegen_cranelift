use std::fmt;

use rustc_codegen_ssa::traits::BackendTypes;
use rustc_target::spec::{HasTargetSpec, Target};

use cranelift_module::Module;

use crate::prelude::*;

pub fn mir_var(loc: Local) -> Variable {
    Variable::with_u32(loc.index() as u32)
}

pub fn pointer_ty(tcx: TyCtxt) -> types::Type {
    match tcx.data_layout.pointer_size.bits() {
        16 => types::I16,
        32 => types::I32,
        64 => types::I64,
        bits => bug!("ptr_sized_integer: unknown pointer bit size {}", bits),
    }
}

fn scalar_to_clif_type(tcx: TyCtxt, scalar: &Scalar) -> Type {
    match scalar.value.size(&tcx).bits() {
        8 => types::I8,
        16 => types::I16,
        32 => types::I32,
        64 => types::I64,
        size => bug!("Unsupported scalar size {}", size),
    }
}

pub fn clif_type_from_ty<'a, 'tcx: 'a>(
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    ty: Ty<'tcx>,
) -> Option<types::Type> {
    Some(match ty.sty {
        ty::Bool => types::I8,
        ty::Uint(size) => match size {
            UintTy::U8 => types::I8,
            UintTy::U16 => types::I16,
            UintTy::U32 => types::I32,
            UintTy::U64 => types::I64,
            UintTy::U128 => unimpl!("u128"),
            UintTy::Usize => pointer_ty(tcx),
        },
        ty::Int(size) => match size {
            IntTy::I8 => types::I8,
            IntTy::I16 => types::I16,
            IntTy::I32 => types::I32,
            IntTy::I64 => types::I64,
            IntTy::I128 => unimpl!("i128"),
            IntTy::Isize => pointer_ty(tcx),
        },
        ty::Char => types::I32,
        ty::Float(size) => match size {
            FloatTy::F32 => types::F32,
            FloatTy::F64 => types::F64,
        },
        ty::FnPtr(_) => pointer_ty(tcx),
        ty::RawPtr(TypeAndMut { ty, mutbl: _ }) | ty::Ref(_, ty, _) => {
            if ty.is_sized(tcx.at(DUMMY_SP), ParamEnv::reveal_all()) {
                pointer_ty(tcx)
            } else {
                return None;
            }
        }
        ty::Param(_) => bug!("{:?}: {:?}", ty, ty.sty),
        _ => return None,
    })
}

pub fn codegen_select(bcx: &mut FunctionBuilder, cond: Value, lhs: Value, rhs: Value) -> Value {
    let lhs_ty = bcx.func.dfg.value_type(lhs);
    let rhs_ty = bcx.func.dfg.value_type(rhs);
    assert_eq!(lhs_ty, rhs_ty);
    if lhs_ty == types::I8 || lhs_ty == types::I16 {
        // FIXME workaround for missing enocding for select.i8
        let lhs = bcx.ins().uextend(types::I32, lhs);
        let rhs = bcx.ins().uextend(types::I32, rhs);
        let res = bcx.ins().select(cond, lhs, rhs);
        bcx.ins().ireduce(lhs_ty, res)
    } else {
        bcx.ins().select(cond, lhs, rhs)
    }
}

fn codegen_field<'a, 'tcx: 'a>(
    fx: &mut FunctionCx<'a, 'tcx, impl Backend>,
    base: Value,
    layout: TyLayout<'tcx>,
    field: mir::Field,
) -> (Value, TyLayout<'tcx>) {
    let field_offset = layout.fields.offset(field.index());
    let field_ty = layout.field(&*fx, field.index());
    if field_offset.bytes() > 0 {
        (
            fx.bcx.ins().iadd_imm(base, field_offset.bytes() as i64),
            field_ty,
        )
    } else {
        (base, field_ty)
    }
}

/// A read-only value
#[derive(Debug, Copy, Clone)]
pub enum CValue<'tcx> {
    ByRef(Value, TyLayout<'tcx>),
    ByVal(Value, TyLayout<'tcx>),
    ByValPair(Value, Value, TyLayout<'tcx>),
}

impl<'tcx> CValue<'tcx> {
    pub fn layout(&self) -> TyLayout<'tcx> {
        match *self {
            CValue::ByRef(_, layout)
            | CValue::ByVal(_, layout)
            | CValue::ByValPair(_, _, layout) => layout,
        }
    }

    pub fn force_stack<'a>(self, fx: &mut FunctionCx<'a, 'tcx, impl Backend>) -> Value
    where
        'tcx: 'a,
    {
        match self {
            CValue::ByRef(value, _layout) => value,
            CValue::ByVal(value, layout) => {
                let stack_slot = fx.bcx.create_stack_slot(StackSlotData {
                    kind: StackSlotKind::ExplicitSlot,
                    size: layout.size.bytes() as u32,
                    offset: None,
                });
                let addr = fx.bcx.ins().stack_addr(fx.pointer_type, stack_slot, 0);
                fx.bcx.ins().store(MemFlags::new(), value, addr, 0);
                addr
            }
            CValue::ByValPair(value, extra, layout) => {
                let stack_slot = fx.bcx.create_stack_slot(StackSlotData {
                    kind: StackSlotKind::ExplicitSlot,
                    size: layout.size.bytes() as u32,
                    offset: None,
                });
                let base = fx.bcx.ins().stack_addr(types::I64, stack_slot, 0);
                let a_addr = codegen_field(fx, base, layout, mir::Field::new(0)).0;
                let b_addr = codegen_field(fx, base, layout, mir::Field::new(1)).0;
                fx.bcx.ins().store(MemFlags::new(), value, a_addr, 0);
                fx.bcx.ins().store(MemFlags::new(), extra, b_addr, 0);
                base
            }
        }
    }

    pub fn load_value<'a>(self, fx: &mut FunctionCx<'a, 'tcx, impl Backend>) -> Value
    where
        'tcx: 'a,
    {
        match self {
            CValue::ByRef(addr, layout) => {
                let clif_ty = fx.clif_type(layout.ty).unwrap_or_else(|| {
                    if layout.ty.is_box()
                        && !fx
                            .layout_of(layout.ty.builtin_deref(true).unwrap().ty)
                            .is_unsized()
                    {
                        // Consider sized box to be a ptr
                        pointer_ty(fx.tcx)
                    } else {
                        panic!("load_value of type {:?}", layout.ty);
                    }
                });
                fx.bcx.ins().load(clif_ty, MemFlags::new(), addr, 0)
            }
            CValue::ByVal(value, _layout) => value,
            CValue::ByValPair(_, _, _layout) => bug!("Please use load_value_pair for ByValPair"),
        }
    }

    pub fn load_value_pair<'a>(self, fx: &mut FunctionCx<'a, 'tcx, impl Backend>) -> (Value, Value)
    where
        'tcx: 'a,
    {
        match self {
            CValue::ByRef(addr, layout) => {
                assert_eq!(
                    layout.size.bytes(),
                    fx.tcx.data_layout.pointer_size.bytes() * 2
                );
                let val1_offset = layout.fields.offset(0).bytes() as i32;
                let val2_offset = layout.fields.offset(1).bytes() as i32;
                let val1 = fx
                    .bcx
                    .ins()
                    .load(fx.pointer_type, MemFlags::new(), addr, val1_offset);
                let val2 = fx
                    .bcx
                    .ins()
                    .load(fx.pointer_type, MemFlags::new(), addr, val2_offset);
                (val1, val2)
            }
            CValue::ByVal(_, _layout) => bug!("Please use load_value for ByVal"),
            CValue::ByValPair(val1, val2, _layout) => (val1, val2),
        }
    }

    pub fn value_field<'a>(
        self,
        fx: &mut FunctionCx<'a, 'tcx, impl Backend>,
        field: mir::Field,
    ) -> CValue<'tcx>
    where
        'tcx: 'a,
    {
        let (base, layout) = match self {
            CValue::ByRef(addr, layout) => (addr, layout),
            _ => bug!("place_field for {:?}", self),
        };

        let (field_ptr, field_layout) = codegen_field(fx, base, layout, field);
        CValue::ByRef(field_ptr, field_layout)
    }

    pub fn unsize_value<'a>(self, fx: &mut FunctionCx<'a, 'tcx, impl Backend>, dest: CPlace<'tcx>) {
        if self.layout().ty == dest.layout().ty {
            dest.write_cvalue(fx, self); // FIXME this shouldn't happen (rust-lang/rust#53602)
            return;
        }
        match &self.layout().ty.sty {
            ty::Ref(_, ty, _) | ty::RawPtr(TypeAndMut { ty, mutbl: _ }) => {
                let (ptr, extra) = match dest.layout().ty.builtin_deref(true).unwrap().ty.sty {
                    ty::Slice(slice_elem_ty) => match ty.sty {
                        ty::Array(array_elem_ty, size) => {
                            assert_eq!(slice_elem_ty, array_elem_ty);
                            let ptr = self.load_value(fx);
                            let extra = fx
                                .bcx
                                .ins()
                                .iconst(fx.pointer_type, size.unwrap_usize(fx.tcx) as i64);
                            (ptr, extra)
                        }
                        _ => bug!("unsize non array {:?} to slice", ty),
                    },
                    ty::Dynamic(data, _) => match ty.sty {
                        ty::Dynamic(_, _) => self.load_value_pair(fx),
                        _ => {
                            let ptr = self.load_value(fx);
                            let vtable = crate::vtable::get_vtable(fx, ty, data.principal());
                            (ptr, vtable)
                        }
                    },
                    _ => bug!(
                        "unsize of type {:?} to {:?}",
                        self.layout().ty,
                        dest.layout().ty
                    ),
                };
                dest.write_cvalue(fx, CValue::ByValPair(ptr, extra, dest.layout()));
            }
            _ => {
                assert!(!self.layout().ty.is_enum(), "Tried to unsize enum");
                let field_count = self.layout().fields.count();
                let mut found_unsize_field = false;
                for idx in 0..field_count {
                    let field_dest = dest.place_field(fx, mir::Field::new(idx));
                    let field_src = self.value_field(fx, mir::Field::new(idx));
                    if field_src.layout().ty.is_phantom_data() {
                        // Ignore PhantomData so for example `Unique<()>` can coerce to `Unique<Debug>`
                        //
                        // ```rust
                        // struct Unique<T: ?Sized> {
                        //     pointer: NonZero<*const T>,
                        //     _marker: PhantomData<T>,
                        // }
                        // ```
                        continue;
                    }
                    if field_src.layout().ty != field_dest.layout().ty {
                        assert!(!found_unsize_field);
                        found_unsize_field = true;
                        field_src.unsize_value(fx, field_dest);
                    } else {
                        field_dest.write_cvalue(fx, field_src);
                    }
                }
            }
        }
    }

    pub fn const_val<'a>(
        fx: &mut FunctionCx<'a, 'tcx, impl Backend>,
        ty: Ty<'tcx>,
        const_val: i64,
    ) -> CValue<'tcx>
    where
        'tcx: 'a,
    {
        let clif_ty = fx.clif_type(ty).unwrap();
        let layout = fx.layout_of(ty);
        CValue::ByVal(fx.bcx.ins().iconst(clif_ty, const_val), layout)
    }

    pub fn unchecked_cast_to(self, layout: TyLayout<'tcx>) -> Self {
        match self {
            CValue::ByRef(addr, _) => CValue::ByRef(addr, layout),
            CValue::ByVal(val, _) => CValue::ByVal(val, layout),
            CValue::ByValPair(val, extra, _) => CValue::ByValPair(val, extra, layout),
        }
    }
}

/// A place where you can write a value to or read a value from
#[derive(Debug, Copy, Clone)]
pub enum CPlace<'tcx> {
    Var(Local, TyLayout<'tcx>),
    Addr(Value, Option<Value>, TyLayout<'tcx>),
}

impl<'a, 'tcx: 'a> CPlace<'tcx> {
    pub fn layout(&self) -> TyLayout<'tcx> {
        match *self {
            CPlace::Var(_, layout) | CPlace::Addr(_, _, layout) => layout,
        }
    }

    pub fn temp(fx: &mut FunctionCx<'a, 'tcx, impl Backend>, ty: Ty<'tcx>) -> CPlace<'tcx> {
        let layout = fx.layout_of(ty);
        assert!(!layout.is_unsized());
        let stack_slot = fx.bcx.create_stack_slot(StackSlotData {
            kind: StackSlotKind::ExplicitSlot,
            size: layout.size.bytes() as u32,
            offset: None,
        });
        CPlace::Addr(
            fx.bcx.ins().stack_addr(fx.pointer_type, stack_slot, 0),
            None,
            layout,
        )
    }

    pub fn from_stack_slot(
        fx: &mut FunctionCx<'a, 'tcx, impl Backend>,
        stack_slot: StackSlot,
        ty: Ty<'tcx>,
    ) -> CPlace<'tcx> {
        let layout = fx.layout_of(ty);
        assert!(!layout.is_unsized());
        CPlace::Addr(
            fx.bcx.ins().stack_addr(fx.pointer_type, stack_slot, 0),
            None,
            layout,
        )
    }

    pub fn to_cvalue(self, fx: &mut FunctionCx<'a, 'tcx, impl Backend>) -> CValue<'tcx> {
        match self {
            CPlace::Var(var, layout) => CValue::ByVal(fx.bcx.use_var(mir_var(var)), layout),
            CPlace::Addr(addr, extra, layout) => {
                assert!(extra.is_none(), "unsized values are not yet supported");
                CValue::ByRef(addr, layout)
            }
        }
    }

    pub fn expect_addr(self) -> Value {
        match self {
            CPlace::Addr(addr, None, _layout) => addr,
            CPlace::Addr(_, _, _) => bug!("Expected sized CPlace::Addr, found {:?}", self),
            CPlace::Var(_, _) => bug!("Expected CPlace::Addr, found CPlace::Var"),
        }
    }

    pub fn write_cvalue(self, fx: &mut FunctionCx<'a, 'tcx, impl Backend>, from: CValue<'tcx>) {
        let from_ty = from.layout().ty;
        let to_ty = self.layout().ty;
        match (&from_ty.sty, &to_ty.sty) {
            (ty::Ref(_, t, src_mut), ty::Ref(_, u, dest_mut))
                if (if *dest_mut != crate::rustc::hir::Mutability::MutImmutable
                    && src_mut != dest_mut
                {
                    false
                } else if t != u {
                    false
                } else {
                    true
                }) =>
            {
                // &mut T -> &T is allowed
                // &'a T -> &'b T is allowed
            }
            (ty::FnPtr(_), ty::FnPtr(_)) => {
                let from_sig = fx.tcx.normalize_erasing_late_bound_regions(
                    ParamEnv::reveal_all(),
                    &from_ty.fn_sig(fx.tcx),
                );
                let to_sig = fx.tcx.normalize_erasing_late_bound_regions(
                    ParamEnv::reveal_all(),
                    &to_ty.fn_sig(fx.tcx),
                );
                assert_eq!(
                    from_sig, to_sig,
                    "Can't write fn ptr with incompatible sig {:?} to place with sig {:?}\n\n{:#?}",
                    from_sig, to_sig, fx,
                );
                // fn(&T) -> for<'l> fn(&'l T) is allowed
            }
            _ => {
                assert_eq!(
                    from_ty,
                    to_ty,
                    "Can't write value with incompatible type {:?} to place with type {:?}\n\n{:#?}",
                    from_ty.sty,
                    to_ty.sty,
                    fx,
                );
            }
        }

        match self {
            CPlace::Var(var, _) => {
                let data = from.load_value(fx);
                fx.bcx.def_var(mir_var(var), data)
            }
            CPlace::Addr(addr, None, dst_layout) => {
                match from {
                    CValue::ByVal(val, _src_layout) => {
                        fx.bcx.ins().store(MemFlags::new(), val, addr, 0);
                    }
                    CValue::ByValPair(val1, val2, _src_layout) => {
                        let val1_offset = dst_layout.fields.offset(0).bytes() as i32;
                        let val2_offset = dst_layout.fields.offset(1).bytes() as i32;
                        fx.bcx.ins().store(MemFlags::new(), val1, addr, val1_offset);
                        fx.bcx.ins().store(MemFlags::new(), val2, addr, val2_offset);
                    }
                    CValue::ByRef(from, _src_layout) => {
                        let size = dst_layout.size.bytes() as i32;
                        // FIXME emit_small_memcpy has a bug as of commit CraneStation/cranelift@b2281ed
                        // fx.bcx.emit_small_memcpy(fx.module.target_config(), addr, from, size, layout.align.abi() as u8, src_layout.align.abi() as u8);

                        let mut offset = 0;
                        while size - offset >= 8 {
                            let byte =
                                fx.bcx
                                    .ins()
                                    .load(fx.pointer_type, MemFlags::new(), from, offset);
                            fx.bcx.ins().store(MemFlags::new(), byte, addr, offset);
                            offset += 8;
                        }
                        while size - offset >= 4 {
                            let byte = fx.bcx.ins().load(types::I32, MemFlags::new(), from, offset);
                            fx.bcx.ins().store(MemFlags::new(), byte, addr, offset);
                            offset += 4;
                        }
                        while offset < size {
                            let byte = fx.bcx.ins().load(types::I8, MemFlags::new(), from, offset);
                            fx.bcx.ins().store(MemFlags::new(), byte, addr, offset);
                            offset += 1;
                        }
                    }
                }
            }
            CPlace::Addr(_, _, _) => bug!("Can't write value to unsized place {:?}", self),
        }
    }

    pub fn place_field(
        self,
        fx: &mut FunctionCx<'a, 'tcx, impl Backend>,
        field: mir::Field,
    ) -> CPlace<'tcx> {
        match self {
            CPlace::Var(var, layout) => {
                bug!(
                    "Tried to project {:?}, which is put in SSA var {:?}",
                    layout.ty,
                    var
                );
            }
            CPlace::Addr(base, extra, layout) => {
                let (field_ptr, field_layout) = codegen_field(fx, base, layout, field);
                let extra = if field_layout.is_unsized() {
                    assert!(extra.is_some());
                    extra
                } else {
                    None
                };
                CPlace::Addr(field_ptr, extra, field_layout)
            }
        }
    }

    pub fn place_index(
        self,
        fx: &mut FunctionCx<'a, 'tcx, impl Backend>,
        index: Value,
    ) -> CPlace<'tcx> {
        let (elem_layout, addr) = match self.layout().ty.sty {
            ty::Array(elem_ty, _) => (fx.layout_of(elem_ty), self.expect_addr()),
            ty::Slice(elem_ty) => (
                fx.layout_of(elem_ty),
                match self {
                    CPlace::Addr(addr, _, _) => addr,
                    CPlace::Var(_, _) => bug!("Expected CPlace::Addr found CPlace::Var"),
                },
            ),
            _ => bug!("place_index({:?})", self.layout().ty),
        };

        let offset = fx
            .bcx
            .ins()
            .imul_imm(index, elem_layout.size.bytes() as i64);

        CPlace::Addr(fx.bcx.ins().iadd(addr, offset), None, elem_layout)
    }

    pub fn place_deref(self, fx: &mut FunctionCx<'a, 'tcx, impl Backend>) -> CPlace<'tcx> {
        let inner_layout = fx.layout_of(self.layout().ty.builtin_deref(true).unwrap().ty);
        if !inner_layout.is_unsized() {
            CPlace::Addr(self.to_cvalue(fx).load_value(fx), None, inner_layout)
        } else {
            match self.layout().abi {
                Abi::ScalarPair(ref a, ref b) => {
                    let addr = self.expect_addr();
                    let ptr =
                        fx.bcx
                            .ins()
                            .load(scalar_to_clif_type(fx.tcx, a), MemFlags::new(), addr, 0);
                    let extra = fx.bcx.ins().load(
                        scalar_to_clif_type(fx.tcx, b),
                        MemFlags::new(),
                        addr,
                        a.value.size(&fx.tcx).bytes() as u32 as i32,
                    );
                    CPlace::Addr(ptr, Some(extra), inner_layout)
                }
                _ => bug!(
                    "Fat ptr doesn't have abi ScalarPair, but it has {:?}",
                    self.layout().abi
                ),
            }
        }
    }

    pub fn write_place_ref(self, fx: &mut FunctionCx<'a, 'tcx, impl Backend>, dest: CPlace<'tcx>) {
        if !self.layout().is_unsized() {
            let ptr = CValue::ByVal(self.expect_addr(), dest.layout());
            dest.write_cvalue(fx, ptr);
        } else {
            match self {
                CPlace::Var(_, _) => bug!("expected CPlace::Addr found CPlace::Var"),
                CPlace::Addr(value, extra, _) => match dest.layout().abi {
                    Abi::ScalarPair(ref a, _) => {
                        fx.bcx
                            .ins()
                            .store(MemFlags::new(), value, dest.expect_addr(), 0);
                        fx.bcx.ins().store(
                            MemFlags::new(),
                            extra.expect("unsized type without metadata"),
                            dest.expect_addr(),
                            a.value.size(&fx.tcx).bytes() as u32 as i32,
                        );
                    }
                    _ => bug!(
                        "Non ScalarPair abi {:?} in write_place_ref dest",
                        dest.layout().abi
                    ),
                },
            }
        }
    }

    pub fn unchecked_cast_to(self, layout: TyLayout<'tcx>) -> Self {
        match self {
            CPlace::Var(var, _) => CPlace::Var(var, layout),
            CPlace::Addr(addr, extra, _) => {
                assert!(!layout.is_unsized());
                CPlace::Addr(addr, extra, layout)
            }
        }
    }

    pub fn downcast_variant(
        self,
        fx: &FunctionCx<'a, 'tcx, impl Backend>,
        variant: VariantIdx,
    ) -> Self {
        let layout = self.layout().for_variant(fx, variant);
        self.unchecked_cast_to(layout)
    }
}

pub fn clif_intcast<'a, 'tcx: 'a>(
    fx: &mut FunctionCx<'a, 'tcx, impl Backend>,
    val: Value,
    to: Type,
    signed: bool,
) -> Value {
    let from = fx.bcx.func.dfg.value_type(val);
    if from == to {
        return val;
    }
    if to.wider_or_equal(from) {
        if signed {
            fx.bcx.ins().sextend(to, val)
        } else {
            fx.bcx.ins().uextend(to, val)
        }
    } else {
        fx.bcx.ins().ireduce(to, val)
    }
}

pub struct FunctionCx<'a, 'tcx: 'a, B: Backend> {
    pub tcx: TyCtxt<'a, 'tcx, 'tcx>,
    pub module: &'a mut Module<B>,
    pub pointer_type: Type, // Cached from module

    pub instance: Instance<'tcx>,
    pub mir: &'tcx Mir<'tcx>,

    pub bcx: FunctionBuilder<'a>,
    pub ebb_map: HashMap<BasicBlock, Ebb>,
    pub local_map: HashMap<Local, CPlace<'tcx>>,

    pub comments: HashMap<Inst, String>,
    pub constants: &'a mut crate::constant::ConstantCx,
    pub caches: &'a mut Caches<'tcx>,

    /// add_global_comment inserts a comment here
    pub top_nop: Option<Inst>,
}

impl<'a, 'tcx: 'a, B: Backend + 'a> fmt::Debug for FunctionCx<'a, 'tcx, B> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "{:?}", self.instance.substs)?;
        writeln!(f, "{:?}", self.local_map)?;

        let mut clif = String::new();
        let mut writer = crate::pretty_clif::CommentWriter(self.comments.clone());
        ::cranelift::codegen::write::decorate_function(
            &mut writer,
            &mut clif,
            &self.bcx.func,
            None,
        )
        .unwrap();
        writeln!(f, "\n{}", clif)
    }
}

impl<'a, 'tcx: 'a, B: Backend> LayoutOf for FunctionCx<'a, 'tcx, B> {
    type Ty = Ty<'tcx>;
    type TyLayout = TyLayout<'tcx>;

    fn layout_of(&self, ty: Ty<'tcx>) -> TyLayout<'tcx> {
        let ty = self.monomorphize(&ty);
        self.tcx.layout_of(ParamEnv::reveal_all().and(&ty)).unwrap()
    }
}

impl<'a, 'tcx, B: Backend + 'a> layout::HasTyCtxt<'tcx> for FunctionCx<'a, 'tcx, B> {
    fn tcx<'b>(&'b self) -> TyCtxt<'b, 'tcx, 'tcx> {
        self.tcx
    }
}

impl<'a, 'tcx, B: Backend + 'a> layout::HasDataLayout for FunctionCx<'a, 'tcx, B> {
    fn data_layout(&self) -> &layout::TargetDataLayout {
        &self.tcx.data_layout
    }
}

impl<'a, 'tcx, B: Backend + 'a> HasTargetSpec for FunctionCx<'a, 'tcx, B> {
    fn target_spec(&self) -> &Target {
        &self.tcx.sess.target.target
    }
}

impl<'a, 'tcx, B: Backend> BackendTypes for FunctionCx<'a, 'tcx, B> {
    type Value = Value;
    type BasicBlock = Ebb;
    type Type = Type;
    type Funclet = !;
    type DIScope = !;
}

impl<'a, 'tcx: 'a, B: Backend + 'a> FunctionCx<'a, 'tcx, B> {
    pub fn monomorphize<T>(&self, value: &T) -> T
    where
        T: TypeFoldable<'tcx>,
    {
        self.tcx.subst_and_normalize_erasing_regions(
            self.instance.substs,
            ty::ParamEnv::reveal_all(),
            value,
        )
    }

    pub fn clif_type(&self, ty: Ty<'tcx>) -> Option<Type> {
        clif_type_from_ty(self.tcx, self.monomorphize(&ty))
    }

    pub fn get_ebb(&self, bb: BasicBlock) -> Ebb {
        *self.ebb_map.get(&bb).unwrap()
    }

    pub fn get_local_place(&mut self, local: Local) -> CPlace<'tcx> {
        *self.local_map.get(&local).unwrap()
    }
}
