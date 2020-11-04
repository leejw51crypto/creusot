use std::collections::HashMap;

use rustc_hir::{def::CtorKind, def_id::DefId};
use rustc_middle::{mir, mir::traversal::preorder, mir::*, ty::AdtDef, ty::TyCtxt};
use rustc_mir::dataflow::{self, impls::MaybeInitializedLocals};

use crate::{
    place::from_place,
    place::{MirPlace, Mutability::*, Projection::*},
    polonius::PoloniusInfo,
};

use crate::mlcfg::{MlCfgExp::*, MlCfgPattern::*, *};

use self::ty::TyTranslator;

mod statement;
mod terminator;

mod ty;

pub struct FunctionTranslator<'a, 'tcx> {
    pub tcx: TyCtxt<'tcx>,
    body: &'a Body<'tcx>,
    pol: PoloniusInfo<'a, 'tcx>,

    // Whether a local is initialized or not at a location
    local_init: dataflow::ResultsCursor<'a, 'tcx, MaybeInitializedLocals>,
    // Tell me the type of the object we are discriminating
    discr_map: HashMap<(BasicBlock, mir::Local), Place<'tcx>>,

    // Current block being generated
    current_block: (Block, Vec<MlCfgStatement>, Option<MlCfgTerminator>),

    past_blocks: Vec<MlCfgBlock>,
}

pub fn translate_tydecl(tcx: TyCtxt, adt: &AdtDef) -> MlTyDecl {
    TyTranslator::new(tcx).translate_tydecl(adt)
}

impl<'a, 'tcx> FunctionTranslator<'a, 'tcx> {
    pub fn new(
        tcx: TyCtxt<'tcx>,
        body: &'a Body<'tcx>,
        pol: PoloniusInfo<'a, 'tcx>,
        local_init: dataflow::ResultsCursor<'a, 'tcx, MaybeInitializedLocals>,
        discr_map: HashMap<(BasicBlock, mir::Local), Place<'tcx>>,
    ) -> Self {
        FunctionTranslator {
            tcx,
            body,
            pol,
            local_init,
            discr_map,
            current_block: (BasicBlock::MAX.into(), Vec::new(), None),
            past_blocks: Vec::new(),
        }
    }

    fn emit_statement(&mut self, s: MlCfgStatement) {
        self.current_block.1.push(s);
    }

    fn emit_terminator(&mut self, t: MlCfgTerminator) {
        assert!(self.current_block.2.is_none());

        self.current_block.2 = Some(t);
    }

    pub fn translate(mut self, nm: DefId) -> MlCfgFunction {
        for (bb, bbd) in preorder(self.body) {
            self.current_block = (bb.into(), vec![], None);

            if bbd.is_cleanup {
                continue;
            }

            let mut loc = bb.start_location();
            for statement in &bbd.statements {
                self.freeze_borrows_dying_at(loc);
                self.translate_statement(statement);
                loc = loc.successor_within_block();
            }

            self.freeze_borrows_dying_at(loc);
            self.translate_terminator(bbd.terminator(), loc);

            self.past_blocks.push(MlCfgBlock {
                label: self.current_block.0,
                statements: self.current_block.1,
                terminator: self.current_block.2.unwrap(),
            });
        }

        let ty_trans = TyTranslator::new(self.tcx);
        let mut vars = self
            .body
            .local_decls
            .iter_enumerated()
            .map(|(loc, decl)| (loc, ty_trans.translate_ty(decl.ty)));
        let retty = vars.next().unwrap().1;

        let name = self.tcx.def_path(nm).to_filename_friendly_no_crate();
        MlCfgFunction {
            name,
            retty,
            args: vars.by_ref().take(self.body.arg_count).collect(),
            vars: vars.collect::<Vec<_>>(),
            blocks: self.past_blocks,
        }
    }

    fn freeze_borrows_dying_at(&mut self, loc: Location) {
        for loan in self.pol.loans_dying_at_start(loc) {
            let orig_loc = self.pol.loan_created_at(loan);
            let local = self.get_local_at(orig_loc);

            self.local_init.seek_before_primary_effect(loc);
            if self.local_init.contains(local) {
                self.emit_statement(MlCfgStatement::Freeze(local));
            }
        }
    }
    // Hacky and ugly replace.
    pub fn get_local_at(&self, loc: Location) -> rustc_middle::mir::Local {
        let stmt = &self.body.basic_blocks()[loc.block].statements[loc.statement_index];

        if let StatementKind::Assign(box (pl, _)) = stmt.kind {
            pl.local
        } else {
            panic!("local not assigned at {:?}", loc);
        }
    }

    // Useful helper to translate an operand
    pub fn translate_operand(&self, operand: &Operand<'tcx>) -> MlCfgExp {
        match operand {
            Operand::Copy(pl) | Operand::Move(pl) => {
                rhs_to_why_exp(&from_place(self.tcx, self.body, pl))
            }
            Operand::Constant(c) => Const(MlCfgConstant::from_mir_constant(self.tcx, c)),
        }
    }
}

fn translate_defid(tcx: TyCtxt, def_id: DefId) -> String {
    tcx.def_path_str(def_id).replace("::", ".")
}

// [(P as Some)]   ---> [_1]
// [(P as Some).0] ---> let Some(a) = [_1] in a
// [(* P)] ---> * [P]
pub fn rhs_to_why_exp(rhs: &MirPlace) -> MlCfgExp {
    let mut inner = Local(rhs.local);

    for proj in rhs.proj.iter() {
        match proj {
            Deref(Mut) => {
                inner = Current(box inner);
            }
            Deref(Not) => {
                // Immutable references are erased in MLCFG
            }
            FieldAccess { ctor, ix, size, field: _, kind } => {
                match kind {
                    // Tuple
                    CtorKind::Fn | CtorKind::Fictive => {
                        let mut pat = vec![Wildcard; *ix];
                        pat.push(VarP("a".into()));
                        pat.append(&mut vec![Wildcard; size - ix - 1]);

                        inner = Let {
                            pattern: ConsP(ctor.to_string(), pat),
                            arg: box inner,
                            body: box Var("a".into()),
                        }
                    }
                    // Unit
                    CtorKind::Const => {
                        assert!(*size == 1 && *ix == 0);
                        unimplemented!();
                    }
                }
            }
            TupleAccess { size, ix } => {
                let mut pat = vec![Wildcard; *ix];
                pat.push(VarP("a".into()));
                pat.append(&mut vec![Wildcard; size - ix - 1]);

                inner = Let { pattern: TupleP(pat), arg: box inner, body: box Var("a".into()) }
            }
        }
    }
    inner
}