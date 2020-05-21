#![allow(dead_code)]

use std::collections::HashMap;
use std::convert::TryFrom;
use std::fs;
use std::pin::Pin;

use super::func::*;

use goscript_vm::ds::{EntIndex, PackageVal, UpValue};
use goscript_vm::null_key;
use goscript_vm::opcode::*;
use goscript_vm::value::*;
use goscript_vm::vm::ByteCode;

use goscript_parser::ast::*;
use goscript_parser::ast_objects::Objects as AstObjects;
use goscript_parser::ast_objects::*;
use goscript_parser::errors::{ErrorList, FilePosErrors};
use goscript_parser::position;
use goscript_parser::token::Token;
use goscript_parser::visitor::{walk_decl, walk_expr, walk_stmt, Visitor};
use goscript_parser::{FileSet, Parser};

macro_rules! current_func_mut {
    ($owner:ident) => {
        &mut $owner.objects.functions[*$owner.func_stack.last().unwrap()]
    };
}

macro_rules! current_func {
    ($owner:ident) => {
        &$owner.objects.functions[*$owner.func_stack.last().unwrap()]
    };
}

/// Built-in functions are not called like normal function for performance reasons
pub struct BuiltInFunc {
    name: &'static str,
    opcode: Opcode,
    params_count: isize,
    variadic: bool,
}

impl BuiltInFunc {
    pub fn new(name: &'static str, op: Opcode, params: isize, variadic: bool) -> BuiltInFunc {
        BuiltInFunc {
            name: name,
            opcode: op,
            params_count: params,
            variadic: variadic,
        }
    }
}

/// CodeGen implements the code generation logic.
pub struct CodeGen<'a> {
    objects: Pin<Box<VMObjects>>,
    ast_objs: &'a AstObjects,
    package_indices: HashMap<String, OpIndex>,
    packages: Vec<PackageKey>,
    current_pkg: PackageKey,
    func_stack: Vec<FunctionKey>,
    built_in_funcs: Vec<BuiltInFunc>,
    built_in_vals: HashMap<&'static str, Opcode>,
    errors: &'a FilePosErrors<'a>,
    blank_ident: IdentKey,
}

impl<'a> Visitor for CodeGen<'a> {
    fn visit_expr(&mut self, expr: &Expr) -> Result<(), ()> {
        walk_expr(self, expr)
    }

    fn visit_stmt(&mut self, stmt: &Stmt) -> Result<(), ()> {
        walk_stmt(self, stmt)
    }

    fn visit_decl(&mut self, decl: &Decl) -> Result<(), ()> {
        walk_decl(self, decl)
    }

    fn visit_expr_ident(&mut self, ident: &IdentKey) -> Result<(), ()> {
        let index = self.resolve_ident(ident)?;
        current_func_mut!(self).emit_load(index);
        Ok(())
    }

    fn visit_expr_ellipsis(&mut self, _els: &Option<Expr>) -> Result<(), ()> {
        unreachable!();
    }

    fn visit_expr_basic_lit(&mut self, blit: &BasicLit) -> Result<(), ()> {
        let val = self.get_const_value(None, &blit)?;
        let func = current_func_mut!(self);
        let i = func.add_const(None, val);
        func.emit_load(i);
        Ok(())
    }

    /// Add function as a const and then generate a closure of it
    fn visit_expr_func_lit(&mut self, flit: &FuncLit) -> Result<(), ()> {
        let fkey = self.gen_func_def(&flit.typ, &flit.body)?;
        let func = current_func_mut!(self);
        let i = func.add_const(None, GosValue::Function(fkey));
        func.emit_load(i);
        func.emit_new();
        Ok(())
    }

    fn visit_expr_composit_lit(&mut self, clit: &CompositeLit) -> Result<(), ()> {
        let val = self.get_comp_value(clit.typ.as_ref().unwrap(), &clit)?;
        let func = current_func_mut!(self);
        let i = func.add_const(None, val);
        func.emit_load(i);
        Ok(())
    }

    fn visit_expr_paren(&mut self) -> Result<(), ()> {
        //unimplemented!();
        Ok(())
    }

    fn visit_expr_selector(&mut self, ident: &IdentKey) -> Result<(), ()> {
        // todo: use index instead of string when Type Checker is in place
        self.gen_push_ident_str(ident);
        current_func_mut!(self).emit_load_field();
        Ok(())
    }

    fn visit_expr_index(&mut self) -> Result<(), ()> {
        current_func_mut!(self).emit_load_field();
        Ok(())
    }

    fn visit_expr_slice(
        &mut self,
        low: &Option<Expr>,
        high: &Option<Expr>,
        max: &Option<Expr>,
    ) -> Result<(), ()> {
        match low {
            None => current_func_mut!(self).emit_code(Opcode::PUSH_NIL),
            Some(e) => self.visit_expr(e)?,
        }
        match high {
            None => current_func_mut!(self).emit_code(Opcode::PUSH_NIL),
            Some(e) => self.visit_expr(e)?,
        }
        match max {
            None => current_func_mut!(self).emit_code(Opcode::SLICE),
            Some(e) => {
                self.visit_expr(e)?;
                current_func_mut!(self).emit_code(Opcode::SLICE_FULL);
            }
        }
        Ok(())
    }

    fn visit_expr_type_assert(&mut self, _typ: &Option<Expr>) -> Result<(), ()> {
        unimplemented!();
    }

    fn visit_expr_call(
        &mut self,
        func: &Expr,
        params: &Vec<Expr>,
        ellipsis: bool,
    ) -> Result<(), ()> {
        // check if this is a built in function first
        if let Expr::Ident(ikey) = func {
            let ident = self.ast_objs.idents[*ikey.as_ref()].clone();
            if ident.entity.into_key().is_none() {
                return if let Some(i) = self.built_in_func_index(&ident.name) {
                    let count = params.iter().map(|e| self.visit_expr(e)).count();
                    let bf = &self.built_in_funcs[i as usize];
                    let func = current_func_mut!(self);
                    func.emit_code(bf.opcode);
                    if bf.variadic {
                        func.emit_data(if ellipsis {
                            0 // do not pack params if there is ellipsis
                        } else {
                            (bf.params_count - 1 - count as isize) as OpIndex
                        });
                    }
                    Ok(())
                } else {
                    self.error_undefined(ident.pos, &ident.name);
                    Err(())
                };
            }
        }

        // normal goscript function
        self.visit_expr(func)?;
        current_func_mut!(self).emit_pre_call();
        let _ = params
            .iter()
            .map(|e| -> Result<(), ()> { self.visit_expr(e) })
            .count();
        // do not pack params if there is ellipsis
        current_func_mut!(self).emit_call(ellipsis);
        Ok(())
    }

    fn visit_expr_star(&mut self) -> Result<(), ()> {
        //todo: could this be a pointer type?
        let code = Opcode::DEREF;
        current_func_mut!(self).emit_code(code);
        Ok(())
    }

    fn visit_expr_unary(&mut self, op: &Token) -> Result<(), ()> {
        let code = match op {
            Token::AND => Opcode::REF,
            Token::ADD => Opcode::UNARY_ADD,
            Token::SUB => Opcode::UNARY_SUB,
            Token::XOR => Opcode::UNARY_XOR,
            _ => unreachable!(),
        };
        current_func_mut!(self).emit_code(code);
        Ok(())
    }

    fn visit_expr_binary(&mut self, left: &Expr, op: &Token, right: &Expr) -> Result<(), ()> {
        self.visit_expr(left)?;
        let code = match op {
            Token::ADD => Opcode::ADD,
            Token::SUB => Opcode::SUB,
            Token::MUL => Opcode::MUL,
            Token::QUO => Opcode::QUO,
            Token::REM => Opcode::REM,
            Token::AND => Opcode::AND,
            Token::OR => Opcode::OR,
            Token::XOR => Opcode::XOR,
            Token::SHL => Opcode::SHL,
            Token::SHR => Opcode::SHR,
            Token::AND_NOT => Opcode::AND_NOT,
            Token::LAND => Opcode::PUSH_FALSE,
            Token::LOR => Opcode::PUSH_TRUE,
            Token::NOT => Opcode::NOT,
            Token::EQL => Opcode::EQL,
            Token::LSS => Opcode::LSS,
            Token::GTR => Opcode::GTR,
            Token::NEQ => Opcode::NEQ,
            Token::LEQ => Opcode::LEQ,
            Token::GEQ => Opcode::GEQ,
            _ => unreachable!(),
        };
        // handles short circuit
        let mark_code = match op {
            Token::LAND => {
                let func = current_func_mut!(self);
                func.emit_code(Opcode::JUMP_IF_NOT);
                func.emit_data(0); // placeholder
                Some((func.code.len(), code))
            }
            Token::LOR => {
                let func = current_func_mut!(self);
                func.emit_code(Opcode::JUMP_IF);
                func.emit_data(0); // placeholder
                Some((func.code.len(), code))
            }
            _ => None,
        };
        self.visit_expr(right)?;
        if let Some((i, c)) = mark_code {
            let func = current_func_mut!(self);
            func.emit_code(Opcode::JUMP);
            func.emit_data(1);
            func.emit_code(c);
            let diff = func.code.len() - i - 1;
            func.code[i - 1] = CodeData::Data(diff as OpIndex);
        } else {
            current_func_mut!(self).emit_code(code);
        }
        Ok(())
    }

    fn visit_expr_key_value(&mut self) -> Result<(), ()> {
        unimplemented!();
    }

    fn visit_expr_array_type(&mut self, arr: &Expr) -> Result<(), ()> {
        let val = self.get_or_gen_type(arr)?;
        let func = current_func_mut!(self);
        let i = func.add_const(None, val);
        func.emit_load(i);
        Ok(())
    }

    fn visit_expr_struct_type(&mut self, _s: &StructType) -> Result<(), ()> {
        unimplemented!();
    }

    fn visit_expr_func_type(&mut self, _s: &FuncType) -> Result<(), ()> {
        unimplemented!();
    }

    fn visit_expr_interface_type(&mut self, _s: &InterfaceType) -> Result<(), ()> {
        unimplemented!();
    }

    fn visit_map_type(&mut self) -> Result<(), ()> {
        unimplemented!();
    }

    fn visit_chan_type(&mut self, _dir: &ChanDir) -> Result<(), ()> {
        unimplemented!();
    }

    fn visit_stmt_decl_gen(&mut self, gdecl: &GenDecl) -> Result<(), ()> {
        for s in gdecl.specs.iter() {
            let spec = &self.ast_objs.specs[*s];
            match spec {
                Spec::Import(_is) => unimplemented!(),
                Spec::Type(ts) => {
                    let ident = self.ast_objs.idents[ts.name].clone();
                    let ident_key = ident.entity.into_key();
                    let typ = self.get_or_gen_type(&ts.typ)?;
                    self.current_func_add_const_def(ident_key.unwrap(), typ);
                }
                Spec::Value(vs) => {
                    if gdecl.token == Token::VAR {
                        let pos = self.ast_objs.idents[vs.names[0]].pos;
                        let lhs = vs
                            .names
                            .iter()
                            .map(|n| -> Result<LeftHandSide, ()> {
                                Ok(LeftHandSide::Primitive(
                                    self.add_local_or_resolve_ident(n, true)?,
                                ))
                            })
                            .collect::<Result<Vec<LeftHandSide>, ()>>()?;
                        self.gen_assign_def_var(&lhs, &vs.values, &vs.typ, None, pos)?;
                    } else {
                        assert!(gdecl.token == Token::CONST);
                        self.gen_def_const(&vs.names, &vs.values, &vs.typ)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn visit_stmt_decl_func(&mut self, fdecl: &FuncDeclKey) -> Result<(), ()> {
        let decl = &self.ast_objs.decls[*fdecl];
        if decl.body.is_none() {
            unimplemented!()
        }
        let stmt = decl.body.as_ref().unwrap();
        // this is a struct method
        if let Some(self_ident) = &decl.recv {
            // insert receiver at be beginning of the params
            let mut ftype = decl.typ.clone();
            let mut fields = self_ident.clone();
            fields.list.append(&mut ftype.params.list);
            ftype.params = fields;
            let fval = GosValue::Function(self.gen_func_def(&ftype, stmt)?);
            let field = &self.ast_objs.fields[self_ident.list[0]];
            let name = &self.ast_objs.idents[decl.name].name;
            let type_val = self.get_or_gen_type(&field.typ)?;
            let mut typ = type_val.get_type_val_mut(&mut self.objects);
            if let GosTypeData::Boxed(b) = typ.data {
                typ = &mut self.objects.types[*b.as_type()];
            }
            typ.add_struct_member(name.clone(), fval);
        } else {
            let fval = GosValue::Function(self.gen_func_def(&decl.typ, stmt)?);
            let ident = &self.ast_objs.idents[decl.name];
            let pkg = &mut self.objects.packages[self.current_pkg];
            let index = pkg.add_member(ident.entity_key().unwrap(), fval);
            if ident.name == "main" {
                pkg.set_main_func(index);
            }
        }
        Ok(())
    }

    fn visit_stmt_labeled(&mut self, _lstmt: &LabeledStmtKey) -> Result<(), ()> {
        unimplemented!();
    }

    fn visit_stmt_send(&mut self, _sstmt: &SendStmt) -> Result<(), ()> {
        unimplemented!();
    }

    fn visit_stmt_incdec(&mut self, idcstmt: &IncDecStmt) -> Result<(), ()> {
        self.gen_assign(&idcstmt.token, &vec![&idcstmt.expr], &vec![], None)?;
        Ok(())
    }

    fn visit_stmt_assign(&mut self, astmt: &AssignStmtKey) -> Result<(), ()> {
        let stmt = &self.ast_objs.a_stmts[*astmt];
        self.gen_assign(
            &stmt.token,
            &stmt.lhs.iter().map(|x| x).collect(),
            &stmt.rhs,
            None,
        )?;
        Ok(())
    }

    fn visit_stmt_go(&mut self, _gostmt: &GoStmt) -> Result<(), ()> {
        unimplemented!();
    }

    fn visit_stmt_defer(&mut self, _dstmt: &DeferStmt) -> Result<(), ()> {
        unimplemented!();
    }

    fn visit_stmt_return(&mut self, rstmt: &ReturnStmt) -> Result<(), ()> {
        for (i, expr) in rstmt.results.iter().enumerate() {
            self.visit_expr(expr)?;
            let f = current_func_mut!(self);
            f.emit_store(
                &LeftHandSide::Primitive(EntIndex::LocalVar(i as OpIndex)),
                -1,
                None,
            );
            f.emit_pop();
        }
        current_func_mut!(self).emit_return();
        Ok(())
    }

    fn visit_stmt_branch(&mut self, _bstmt: &BranchStmt) -> Result<(), ()> {
        unimplemented!();
    }

    fn visit_stmt_block(&mut self, bstmt: &BlockStmt) -> Result<(), ()> {
        for stmt in bstmt.list.iter() {
            self.visit_stmt(stmt)?;
        }
        Ok(())
    }

    fn visit_stmt_if(&mut self, ifstmt: &IfStmt) -> Result<(), ()> {
        if let Some(init) = &ifstmt.init {
            self.visit_stmt(init)?;
        }
        self.visit_expr(&ifstmt.cond)?;
        let func = current_func_mut!(self);
        func.emit_code(Opcode::JUMP_IF_NOT);
        // place holder, to be set later
        func.emit_data(0);
        let top_marker = func.code.len();

        drop(func);
        self.visit_stmt_block(&ifstmt.body)?;
        let marker_if_arm_end = if ifstmt.els.is_some() {
            let func = current_func_mut!(self);
            func.emit_code(Opcode::JUMP);
            // place holder, to be set later
            func.emit_data(0);
            Some(func.code.len())
        } else {
            None
        };

        // set the correct else jump target
        let func = current_func_mut!(self);
        // todo: don't crash if OpIndex overflows
        let offset = i16::try_from(func.code.len() - top_marker).unwrap();
        func.code[top_marker - 1] = CodeData::Data(offset);

        if let Some(els) = &ifstmt.els {
            self.visit_stmt(els)?;
            // set the correct if_arm_end jump target
            let func = current_func_mut!(self);
            let marker = marker_if_arm_end.unwrap();
            // todo: don't crash if OpIndex overflows
            let offset = i16::try_from(func.code.len() - marker).unwrap();
            func.code[marker - 1] = CodeData::Data(offset);
        }
        Ok(())
    }

    fn visit_stmt_case(&mut self, _cclause: &CaseClause) -> Result<(), ()> {
        unimplemented!();
    }

    fn visit_stmt_switch(&mut self, _sstmt: &SwitchStmt) -> Result<(), ()> {
        unimplemented!();
    }

    fn visit_stmt_type_switch(&mut self, _tstmt: &TypeSwitchStmt) -> Result<(), ()> {
        unimplemented!();
    }

    fn visit_stmt_comm(&mut self, _cclause: &CommClause) -> Result<(), ()> {
        unimplemented!();
    }

    fn visit_stmt_select(&mut self, _sstmt: &SelectStmt) -> Result<(), ()> {
        unimplemented!();
    }

    fn visit_stmt_for(&mut self, fstmt: &ForStmt) -> Result<(), ()> {
        if let Some(init) = &fstmt.init {
            self.visit_stmt(init)?;
        }
        let top_marker = current_func!(self).code.len();
        let out_marker = if let Some(cond) = &fstmt.cond {
            self.visit_expr(&cond)?;
            let func = current_func_mut!(self);
            func.emit_code(Opcode::JUMP_IF_NOT);
            // place holder, to be set later
            func.emit_data(0);
            Some(func.code.len())
        } else {
            None
        };
        self.visit_stmt_block(&fstmt.body)?;
        if let Some(post) = &fstmt.post {
            self.visit_stmt(post)?;
        }

        // jump to the top
        let func = current_func_mut!(self);
        func.emit_code(Opcode::JUMP);
        // todo: don't crash if OpIndex overflows
        let offset = i16::try_from(-((func.code.len() + 1 - top_marker) as isize)).unwrap();
        func.emit_data(offset);

        // set the correct else jump out target
        if let Some(m) = out_marker {
            let func = current_func_mut!(self);
            // todo: don't crash if OpIndex overflows
            let offset = i16::try_from(func.code.len() - m).unwrap();
            func.code[m - 1] = CodeData::Data(offset);
        }
        Ok(())
    }

    fn visit_stmt_range(&mut self, rstmt: &RangeStmt) -> Result<(), ()> {
        let blank = Expr::Ident(Box::new(self.blank_ident));
        let lhs = vec![
            rstmt.key.as_ref().unwrap_or(&blank),
            rstmt.val.as_ref().unwrap_or(&blank),
        ];
        let marker = self
            .gen_assign(&rstmt.token, &lhs, &vec![], Some(&rstmt.expr))?
            .unwrap();

        self.visit_stmt_block(&rstmt.body)?;
        // jump to the top
        let func = current_func_mut!(self);
        func.emit_code(Opcode::JUMP);
        // todo: don't crash if OpIndex overflows
        let offset = i16::try_from(-((func.code.len() + 1 - marker) as isize)).unwrap();
        func.emit_data(offset);
        // now tell Opcode::RANGE where to jump after it's done
        // todo: don't crash if OpIndex overflows
        let end_offset = i16::try_from(func.code.len() - (marker + 2)).unwrap();
        func.code[marker + 1] = CodeData::Data(end_offset);
        Ok(())
    }
}

impl<'a> CodeGen<'a> {
    pub fn new(aobjects: &'a AstObjects, err: &'a FilePosErrors, bk: IdentKey) -> CodeGen<'a> {
        let funcs = vec![
            BuiltInFunc::new("new", Opcode::NEW, 1, false),
            BuiltInFunc::new("make", Opcode::MAKE, 2, true),
            BuiltInFunc::new("len", Opcode::LEN, 1, false),
            BuiltInFunc::new("cap", Opcode::CAP, 1, false),
            BuiltInFunc::new("append", Opcode::APPEND, 2, true),
            BuiltInFunc::new("assert", Opcode::ASSERT, 1, false),
        ];
        let mut vals = HashMap::new();
        vals.insert("true", Opcode::PUSH_TRUE);
        vals.insert("false", Opcode::PUSH_FALSE);
        vals.insert("nil", Opcode::PUSH_NIL);
        CodeGen {
            objects: Box::pin(VMObjects::new()),
            ast_objs: aobjects,
            package_indices: HashMap::new(),
            packages: Vec::new(),
            current_pkg: null_key!(),
            func_stack: Vec::new(),
            built_in_funcs: funcs,
            built_in_vals: vals,
            errors: err,
            blank_ident: bk,
        }
    }

    fn resolve_ident(&mut self, ident: &IdentKey) -> Result<EntIndex, ()> {
        let id = &self.ast_objs.idents[*ident];
        // 0. try built-ins
        if id.entity_key().is_none() {
            if let Some(op) = self.built_in_vals.get(&*id.name) {
                return Ok(EntIndex::BuiltIn(*op));
            } else {
                self.error_undefined(id.pos, &id.name);
                return Err(());
            }
        }

        // 1. try local frist
        let entity_key = id.entity_key().unwrap();
        let local = current_func_mut!(self)
            .entity_index(&entity_key)
            .map(|x| *x);
        if local.is_some() {
            return Ok(local.unwrap());
        }
        // 2. try upvalue
        let upvalue = self
            .func_stack
            .clone()
            .iter()
            .skip(1) // skip package constructor
            .rev()
            .skip(1) // skip itself
            .find_map(|ifunc| {
                let f = &mut self.objects.functions[*ifunc];
                let index = f.entity_index(&entity_key).map(|x| *x);
                if let Some(ind) = index {
                    Some(UpValue::Open(*ifunc, ind.into()))
                } else {
                    None
                }
            });
        if let Some(uv) = upvalue {
            let func = current_func_mut!(self);
            let index = func.try_add_upvalue(&entity_key, uv);
            return Ok(index);
        }
        // 3. try the package level
        let pkg = &self.objects.packages[self.current_pkg];
        if let Some(index) = pkg.get_member_index(&entity_key) {
            return Ok(EntIndex::PackageMember(*index));
        }

        unreachable!();
    }

    fn add_local_or_resolve_ident(
        &mut self,
        ikey: &IdentKey,
        is_def: bool,
    ) -> Result<EntIndex, ()> {
        let ident = self.ast_objs.idents[*ikey].clone();
        let func = current_func_mut!(self);
        if ident.is_blank() {
            Ok(EntIndex::Blank)
        } else if is_def {
            let ident_key = ident.entity.into_key();
            let index = func.add_local(ident_key.clone());
            if func.is_ctor() {
                let pkg_key = func.package;
                let pkg = &mut self.objects.packages[pkg_key];
                pkg.add_var(ident_key.unwrap(), index.into());
            }
            Ok(index)
        } else {
            self.resolve_ident(ikey)
        }
    }

    fn gen_def_const(
        &mut self,
        names: &Vec<IdentKey>,
        values: &Vec<Expr>,
        typ: &Option<Expr>,
    ) -> Result<(), ()> {
        if names.len() != values.len() {
            let ident = &self.ast_objs.idents[names[0]];
            self.error_mismatch(ident.pos, names.len(), values.len());
            return Err(());
        }
        for i in 0..names.len() {
            let ident = self.ast_objs.idents[names[i]].clone();
            let val = self.value_from_basic_literal(typ.as_ref(), &values[i])?;
            let ident_key = ident.entity.into_key();
            self.current_func_add_const_def(ident_key.unwrap(), val);
        }
        Ok(())
    }

    /// entrance for all assign related stmts
    /// var x
    /// x := 0
    /// x += 1
    /// x++
    /// for x := range xxx
    fn gen_assign(
        &mut self,
        token: &Token,
        lhs_exprs: &Vec<&Expr>,
        rhs_exprs: &Vec<Expr>,
        range: Option<&Expr>,
    ) -> Result<Option<usize>, ()> {
        let is_def = *token == Token::DEFINE;
        let pos = lhs_exprs[0].pos(&self.ast_objs);
        let lhs = lhs_exprs
            .iter()
            .map(|expr| match expr {
                Expr::Ident(ident) => {
                    let idx = self.add_local_or_resolve_ident(ident.as_ref(), is_def)?;
                    Ok(LeftHandSide::Primitive(idx))
                }
                Expr::Index(ind_expr) => {
                    self.visit_expr(&ind_expr.as_ref().expr)?;
                    self.visit_expr(&ind_expr.as_ref().index)?;
                    Ok(LeftHandSide::IndexSelExpr(0)) // the true index will be calculated later
                }
                Expr::Selector(sexpr) => {
                    self.visit_expr(&sexpr.expr)?;
                    self.gen_push_ident_str(&sexpr.sel);
                    Ok(LeftHandSide::IndexSelExpr(0)) // the true index will be calculated later
                }
                Expr::Star(sexpr) => {
                    self.visit_expr(&sexpr.expr)?;
                    Ok(LeftHandSide::Deref(0)) // the true index will be calculated later
                }
                _ => unreachable!(),
            })
            .collect::<Result<Vec<LeftHandSide>, ()>>()?;

        let simple_op = match token {
            Token::ADD_ASSIGN | Token::INC => Some(Opcode::ADD), // +=
            Token::SUB_ASSIGN | Token::DEC => Some(Opcode::SUB), // -=
            Token::MUL_ASSIGN => Some(Opcode::MUL),              // *=
            Token::QUO_ASSIGN => Some(Opcode::QUO),              // /=
            Token::REM_ASSIGN => Some(Opcode::REM),              // %=
            Token::AND_ASSIGN => Some(Opcode::AND),              // &=
            Token::OR_ASSIGN => Some(Opcode::OR),                // |=
            Token::XOR_ASSIGN => Some(Opcode::XOR),              // ^=
            Token::SHL_ASSIGN => Some(Opcode::SHL),              // <<=
            Token::SHR_ASSIGN => Some(Opcode::SHR),              // >>=
            Token::AND_NOT_ASSIGN => Some(Opcode::AND_NOT),      // &^=

            Token::ASSIGN | Token::DEFINE => None,
            _ => unreachable!(),
        };
        if let Some(code) = simple_op {
            if *token == Token::INC || *token == Token::DEC {
                self.gen_op_assign(&lhs[0], code as OpIndex, None)?;
            } else {
                assert_eq!(lhs_exprs.len(), 1);
                assert_eq!(rhs_exprs.len(), 1);
                self.gen_op_assign(&lhs[0], code as OpIndex, Some(&rhs_exprs[0]))?;
            }
            Ok(None)
        } else {
            self.gen_assign_def_var(&lhs, &rhs_exprs, &None, range, pos)
        }
    }

    fn gen_assign_def_var(
        &mut self,
        lhs: &Vec<LeftHandSide>,
        values: &Vec<Expr>,
        typ: &Option<Expr>,
        range: Option<&Expr>,
        pos: position::Pos,
    ) -> Result<Option<usize>, ()> {
        let mut range_marker = None;
        // handle the right hand side
        if let Some(r) = range {
            // the range statement
            self.visit_expr(r)?;
            let func = current_func_mut!(self);
            func.emit_code(Opcode::PUSH_IMM);
            func.emit_data(-1);
            range_marker = Some(func.code.len());
            func.emit_range();
            func.emit_data(0); // placeholder, the block_end address.
        } else if values.len() == lhs.len() {
            // define or assign with values
            for val in values.iter() {
                self.visit_expr(val)?;
            }
        } else if values.len() == 0 {
            // define without values
            let val = self.get_type_default(&typ.as_ref().unwrap())?;
            for _ in 0..lhs.len() {
                let func = current_func_mut!(self);
                let i = func.add_const(None, val);
                func.emit_load(i);
            }
        } else if values.len() == 1 {
            // define or assign with function call on the right
            if let Expr::Call(_) = values[0] {
                self.visit_expr(&values[0])?;
            } else {
                self.error_mismatch(pos, lhs.len(), values.len());
                return Err(());
            }
        } else {
            self.error_mismatch(pos, lhs.len(), values.len());
            return Err(());
        }

        // now the values should be on stack, generate code to set them to the lhs
        let func = current_func_mut!(self);
        let total_lhs_val = lhs.iter().fold(0, |acc, x| match x {
            LeftHandSide::Primitive(_) => acc,
            LeftHandSide::IndexSelExpr(_) => acc + 2,
            LeftHandSide::Deref(_) => acc + 1,
        });
        let total_rhs_val = lhs.len() as i16;
        let total_val = (total_lhs_val + total_rhs_val) as i16;
        let mut current_indexing_index = -total_val;
        for (i, l) in lhs.iter().enumerate() {
            let val_index = i as i16 - total_rhs_val;
            match l {
                LeftHandSide::Primitive(_) => {
                    func.emit_store(l, val_index, None);
                }
                LeftHandSide::IndexSelExpr(_) => {
                    func.emit_store(
                        &LeftHandSide::IndexSelExpr(current_indexing_index),
                        val_index,
                        None,
                    );
                    // the lhs of IndexSelExpr takes two spots
                    current_indexing_index += 2;
                }
                LeftHandSide::Deref(_) => {
                    func.emit_store(
                        &LeftHandSide::Deref(current_indexing_index),
                        val_index,
                        None,
                    );
                    current_indexing_index += 1;
                }
            }
        }
        for _ in 0..total_val {
            func.emit_pop();
        }
        Ok(range_marker)
    }

    fn gen_op_assign(
        &mut self,
        left: &LeftHandSide,
        op: OpIndex,
        right: Option<&Expr>,
    ) -> Result<(), ()> {
        if let Some(e) = right {
            self.visit_expr(e)?;
        } else {
            // it's inc/dec
            let func = current_func_mut!(self);
            func.emit_code(Opcode::PUSH_IMM);
            func.emit_data(1);
        }
        let func = current_func_mut!(self);
        match left {
            LeftHandSide::Primitive(_) => {
                // why no magic number?
                // local index is resolved in gen_assign
                func.emit_store(left, -1, Some(op));
            }
            LeftHandSide::IndexSelExpr(_) => {
                // why -3?  stack looks like this(bottom to top) :
                //  [... target, index, value]
                func.emit_store(&LeftHandSide::IndexSelExpr(-3), -1, Some(op));
            }
            LeftHandSide::Deref(_) => {
                // why -2?  stack looks like this(bottom to top) :
                //  [... target, value]
                func.emit_store(&LeftHandSide::Deref(-2), -1, Some(op));
            }
        }
        func.emit_pop();
        Ok(())
    }

    fn gen_func_def(&mut self, typ: &FuncType, body: &BlockStmt) -> Result<FunctionKey, ()> {
        let ftype = self.get_or_gen_type(&Expr::Func(Box::new(typ.clone())))?;
        let type_val = ftype.get_type_val(&self.objects);
        let params = type_val.get_closure_params();
        let variadic = params.len() > 0
            && params[params.len() - 1]
                .get_type_val(&self.objects)
                .is_variadic();
        let fkey = *GosValue::new_function(
            self.current_pkg.clone(),
            *ftype.as_type(),
            variadic,
            false,
            &mut self.objects,
        )
        .as_function();
        let mut func = &mut self.objects.functions[fkey];
        func.ret_count = match &typ.results {
            Some(fl) => func.add_params(&fl, self.ast_objs, self.errors)?,
            None => 0,
        };
        func.param_count = func.add_params(&typ.params, self.ast_objs, self.errors)?;

        self.func_stack.push(fkey.clone());
        // process function body
        self.visit_stmt_block(body)?;
        // it will not be executed if it's redundant
        self.objects.functions[fkey].emit_return();

        self.func_stack.pop();
        Ok(fkey)
    }

    fn value_from_literal(&mut self, typ: Option<&Expr>, expr: &Expr) -> Result<GosValue, ()> {
        match typ {
            Some(type_expr) => match type_expr {
                Expr::Array(_) | Expr::Map(_) | Expr::Struct(_) => {
                    self.value_from_comp_literal(type_expr, expr)
                }
                _ => self.value_from_basic_literal(typ, expr),
            },
            None => self.value_from_basic_literal(None, expr),
        }
    }

    fn value_from_basic_literal(
        &mut self,
        typ: Option<&Expr>,
        expr: &Expr,
    ) -> Result<GosValue, ()> {
        match expr {
            Expr::BasicLit(lit) => self.get_const_value(typ, lit),
            _ => {
                dbg!(expr);
                self.errors.add_str(
                    expr.pos(self.ast_objs),
                    "complex constant not supported yet",
                );
                Err(())
            }
        }
    }

    // this is a simplified version of Go's constant evaluation, which needs to be implemented
    // as part of the Type Checker
    fn get_const_value(&mut self, typ: Option<&Expr>, blit: &BasicLit) -> Result<GosValue, ()> {
        match typ {
            None => {
                let val = match &blit.token {
                    Token::INT(i) => GosValue::Int(i.parse::<isize>().unwrap()),
                    Token::FLOAT(f) => GosValue::Float64(f.parse::<f64>().unwrap()),
                    Token::IMAG(_) => unimplemented!(),
                    Token::CHAR(c) => GosValue::Int(c.chars().skip(1).next().unwrap() as isize),
                    Token::STRING(s) => {
                        GosValue::new_str(s[1..s.len() - 1].to_string(), &mut self.objects.strings)
                    }
                    _ => unreachable!(),
                };
                Ok(val)
            }
            Some(t) => {
                let type_val = self.get_type_default(t)?;
                match (type_val, &blit.token) {
                    (GosValue::Int(_), Token::FLOAT(f)) => {
                        let fval = f.parse::<f64>().unwrap();
                        if fval.fract() != 0.0 {
                            self.errors
                                .add(blit.pos, format!("constant {} truncated to integer", f));
                            Err(())
                        } else if (fval.round() as isize) > std::isize::MAX
                            || (fval.round() as isize) < std::isize::MIN
                        {
                            self.errors.add(blit.pos, format!("{} overflows int", f));
                            Err(())
                        } else {
                            Ok(GosValue::Int(fval.round() as isize))
                        }
                    }
                    (GosValue::Int(_), Token::INT(ilit)) => match ilit.parse::<isize>() {
                        Ok(i) => Ok(GosValue::Int(i)),
                        Err(_) => {
                            self.errors.add(blit.pos, format!("{} overflows int", ilit));
                            Err(())
                        }
                    },
                    (GosValue::Int(_), Token::CHAR(c)) => {
                        Ok(GosValue::Int(c.chars().skip(1).next().unwrap() as isize))
                    }
                    (GosValue::Float64(_), Token::FLOAT(f)) => {
                        Ok(GosValue::Float64(f.parse::<f64>().unwrap()))
                    }
                    (GosValue::Float64(_), Token::INT(i)) => {
                        Ok(GosValue::Float64(i.parse::<f64>().unwrap()))
                    }
                    (GosValue::Float64(_), Token::CHAR(c)) => Ok(GosValue::Float64(
                        c.chars().skip(1).next().unwrap() as isize as f64,
                    )),
                    (GosValue::Str(_), Token::STRING(s)) => {
                        Ok(GosValue::new_str(s.to_string(), &mut self.objects.strings))
                    }
                    (_, _) => {
                        self.errors.add_str(blit.pos, "invalid constant literal");
                        Err(())
                    }
                }
            }
        }
    }

    fn get_or_gen_type(&mut self, expr: &Expr) -> Result<GosValue, ()> {
        match expr {
            Expr::Ident(ikey) => {
                let ident = &self.ast_objs.idents[*ikey.as_ref()];
                match self.objects.basic_type(&ident.name) {
                    Some(val) => Ok(val.clone()),
                    None => {
                        let i = self.resolve_ident(ikey)?;
                        match i {
                            EntIndex::Const(i) => {
                                let func = current_func_mut!(self);
                                Ok(func.const_val(i.into()).clone())
                            }
                            EntIndex::PackageMember(i) => {
                                let pkg = &self.objects.packages[self.current_pkg];
                                Ok(*pkg.member(i))
                            }
                            _ => unreachable!(),
                        }
                    }
                }
            }
            Expr::Array(atype) => {
                let vtype = self.get_or_gen_type(&atype.elt)?;
                Ok(GosType::new_slice(vtype, &mut self.objects))
            }
            Expr::Map(mtype) => {
                let ktype = self.get_or_gen_type(&mtype.key)?;
                let vtype = self.get_or_gen_type(&mtype.val)?;
                Ok(GosType::new_map(ktype, vtype, &mut self.objects))
            }
            Expr::Struct(stype) => {
                let mut fields = Vec::new();
                let mut map = HashMap::<String, OpIndex>::new();
                let mut i = 0;
                for f in stype.fields.list.iter() {
                    let field = &self.ast_objs.fields[*f];
                    let typ = self.get_or_gen_type(&field.typ)?;
                    for name in &field.names {
                        fields.push(typ);
                        map.insert(self.ast_objs.idents[*name].name.clone(), i);
                        i += 1;
                    }
                }
                Ok(GosType::new_struct(fields, map, &mut self.objects))
            }
            Expr::Interface(itype) => {
                let methods = itype
                    .methods
                    .list
                    .iter()
                    .map(|x| {
                        let field = &self.ast_objs.fields[*x];
                        self.get_or_gen_type(&field.typ)
                    })
                    .collect::<Result<Vec<GosValue>, ()>>()?;
                Ok(GosType::new_interface(methods, &mut self.objects))
            }
            Expr::Func(ftype) => {
                let params = ftype
                    .params
                    .list
                    .iter()
                    .map(|x| {
                        let field = &self.ast_objs.fields[*x];
                        self.get_or_gen_type(&field.typ)
                    })
                    .collect::<Result<Vec<GosValue>, ()>>()?;
                let results = match &ftype.results {
                    Some(re) => re
                        .list
                        .iter()
                        .map(|x| {
                            let field = &self.ast_objs.fields[*x];
                            self.get_or_gen_type(&field.typ)
                        })
                        .collect::<Result<Vec<GosValue>, ()>>()?,
                    None => Vec::new(),
                };
                Ok(GosType::new_closure(params, results, &mut self.objects))
            }
            Expr::Star(sexpr) => {
                let inner = self.get_or_gen_type(&sexpr.expr)?;
                Ok(GosType::new_boxed(inner, &mut self.objects))
            }
            Expr::Ellipsis(eexpr) => {
                let elt = self.get_or_gen_type(eexpr.elt.as_ref().unwrap())?;
                Ok(GosType::new_variadic(elt, &mut self.objects))
            }
            Expr::Chan(_ctype) => unimplemented!(),
            _ => unreachable!(),
        }
    }

    fn get_type_default(&mut self, expr: &Expr) -> Result<GosValue, ()> {
        let typ = self.get_or_gen_type(expr)?;
        let typ_val = &self.objects.types[*typ.as_type()];
        Ok(typ_val.zero_val().clone())
    }

    fn value_from_comp_literal(&mut self, typ: &Expr, expr: &Expr) -> Result<GosValue, ()> {
        match expr {
            Expr::CompositeLit(lit) => self.get_comp_value(typ, lit),
            _ => unreachable!(),
        }
    }

    fn get_comp_value(&mut self, typ: &Expr, literal: &CompositeLit) -> Result<GosValue, ()> {
        match typ {
            Expr::Array(arr) => {
                if arr.as_ref().len.is_some() {
                    // array is not supported yet
                    unimplemented!()
                }
                let vals = literal
                    .elts
                    .iter()
                    .map(|elt| self.value_from_literal(Some(&arr.as_ref().elt), elt))
                    .collect::<Result<Vec<GosValue>, ()>>()?;
                Ok(GosValue::with_slice_val(vals, &mut self.objects.slices))
            }
            Expr::Map(map) => {
                let key_vals = literal
                    .elts
                    .iter()
                    .map(|etl| {
                        if let Expr::KeyValue(kv) = etl {
                            let k = self.value_from_literal(Some(&map.key), &kv.as_ref().key)?;
                            let v = self.value_from_literal(Some(&map.val), &kv.as_ref().val)?;
                            Ok((k, v))
                        } else {
                            unreachable!()
                        }
                    })
                    .collect::<Result<Vec<(GosValue, GosValue)>, ()>>()?;
                let val = self.get_type_default(&map.val)?;
                let map = GosValue::new_map(val, &mut self.objects);
                for kv in key_vals.iter() {
                    self.objects.maps[*map.as_map()].insert(kv.0, kv.1);
                }
                Ok(map)
            }
            _ => unimplemented!(),
        }
    }

    fn current_func_add_const_def(&mut self, entity: EntityKey, cst: GosValue) -> EntIndex {
        let func = current_func_mut!(self);
        let index = func.add_const(Some(entity.clone()), cst);
        if func.is_ctor() {
            let pkg_key = func.package;
            drop(func);
            let pkg = &mut self.objects.packages[pkg_key];
            pkg.add_member(entity, cst);
        }
        index
    }

    fn gen_push_ident_str(&mut self, ident: &IdentKey) {
        let name = self.ast_objs.idents[*ident].name.clone();
        let gos_val = GosValue::new_str(name, &mut self.objects.strings);
        let func = current_func_mut!(self);
        let index = func.add_const(None, gos_val);
        func.emit_load(index);
    }

    fn built_in_func_index(&self, name: &str) -> Option<OpIndex> {
        self.built_in_funcs.iter().enumerate().find_map(|(i, x)| {
            if x.name == name {
                Some(i as OpIndex)
            } else {
                None
            }
        })
    }

    fn error_undefined(&self, pos: position::Pos, name: &String) {
        self.errors.add(pos, format!("undefined: {}", name));
    }

    fn error_mismatch(&self, pos: position::Pos, l: usize, r: usize) {
        self.errors.add(
            pos,
            format!("assignment mismatch: {} variables but {} values", l, r),
        )
    }

    fn error_type(&self, pos: position::Pos, msg: &str) {
        self.errors.add(
            pos,
            format!(
                "type error(should be caught by Type Checker when it's in place): {}",
                msg
            ),
        )
    }

    fn gen(&mut self, f: File) -> Result<(), ()> {
        let pkg_name = &self.ast_objs.idents[f.name].name;
        let pkg_val = PackageVal::new(pkg_name.clone());
        let pkey = self.objects.packages.insert(pkg_val);
        let ftype = self.objects.default_closure_type.unwrap();
        let fkey = *GosValue::new_function(pkey, *ftype.as_type(), false, true, &mut self.objects)
            .as_function();
        // the 0th member is the constructor
        self.objects.packages[pkey].add_member(null_key!(), GosValue::Function(fkey));

        self.packages.push(pkey);
        let index = self.packages.len() as i16 - 1;
        self.package_indices.insert(pkg_name.clone(), index);
        self.current_pkg = pkey;

        self.func_stack.push(fkey.clone());
        for d in f.decls.iter() {
            self.visit_decl(d)?;
        }
        let func = &mut self.objects.functions[fkey];
        func.emit_return_init_pkg(index);
        self.func_stack.pop();
        Ok(())
    }

    // generate the entry function for ByteCode
    fn gen_entry(&mut self) -> FunctionKey {
        // import the 0th pkg and call the main function of the pkg
        let ftype = self.objects.default_closure_type.unwrap();
        let fkey = *GosValue::new_function(
            null_key!(),
            *ftype.as_type(),
            false,
            false,
            &mut self.objects,
        )
        .as_function();
        let func = &mut self.objects.functions[fkey];
        func.emit_import(0);
        func.emit_code(Opcode::PUSH_IMM);
        func.emit_data(-1);
        func.emit_load_field();
        func.emit_pre_call();
        func.emit_call(false);
        func.emit_return();
        fkey
    }

    pub fn into_byte_code(mut self) -> ByteCode {
        let entry = self.gen_entry();
        ByteCode {
            objects: self.objects,
            package_indices: self.package_indices,
            packages: self.packages,
            entry: entry,
        }
    }

    pub fn load_parse_gen(path: &str, trace: bool) -> Result<ByteCode, usize> {
        let mut astobjs = AstObjects::new();
        let mut fset = FileSet::new();
        let el = ErrorList::new();
        let src = fs::read_to_string(path).expect("read file err: ");
        let pfile = fset.add_file(path, None, src.chars().count());
        let afile = Parser::new(&mut astobjs, pfile, &el, &src, trace).parse_file();
        if el.len() > 0 {
            print!("parsing failed:\n");
            print!("\n<- {} ->\n", el);
            return Err(el.len());
        }

        let pos_err = FilePosErrors::new(pfile, &el);
        let blank_ident = astobjs.idents.insert(Ident::blank(0));
        let mut code_gen = CodeGen::new(&mut astobjs, &pos_err, blank_ident);
        let re = code_gen.gen(afile.unwrap());
        if re.is_err() {
            print!("code gen failed:\n");
            print!("\n<- {} ->\n", el);
            Err(el.len())
        } else {
            Ok(code_gen.into_byte_code())
        }
    }
}