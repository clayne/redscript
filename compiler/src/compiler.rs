use std::collections::VecDeque;
use std::mem;
use std::ops::Not;
use std::rc::Rc;
use std::str::FromStr;

use ahash::RandomState;
use hashbrown::{HashMap, HashSet};
use itertools::{chain, Itertools};
use redscript::ast::{Expr, Seq, SourceAst, Span};
use redscript::bundle::{ConstantPool, PoolIndex};
use redscript::bytecode::{Instr, Intrinsic};
use redscript::definition::{
    AnyDefinition, Class as PoolClass, ClassFlags, Enum as PoolEnum, Field as PoolField, FieldFlags,
    Function as PoolFunction, FunctionFlags, ParameterFlags, Type as PoolType, Visibility,
};
use redscript::Str;
use sequence_trie::SequenceTrie;

use crate::autobox::{Autobox, Boxable};
use crate::codegen::builders::{ClassBuilder, EnumBuilder, FieldBuilder, FunctionBuilder, ParamBuilder, TypeCache};
use crate::codegen::{names, CodeGen, LocalIndices};
use crate::error::{CompileError, CompileResult, ParseError, TypeError, Unsupported};
use crate::parser::{
    self, AnnotationKind, ClassSource, EnumSource, FunctionSource, Import, MemberSource, ModulePath, ParameterSource,
    Qualifier, Qualifiers, SourceEntry, SourceModule,
};
use crate::scoped_map::ScopedMap;
use crate::source_map::Files;
use crate::sugar::Desugar;
use crate::type_repo::*;
use crate::typer::*;
use crate::{IndexMap, StringInterner};

#[derive(Debug)]
pub struct Compiler<'id> {
    repo: TypeRepo<'id>,
    interner: &'id StringInterner,
    defined_types: Vec<TypeId<'id>>,
    modules: ModuleMap<'id>,
    compile_queue: Vec<Module<'id>>,
    reporter: ErrorReporter<'id>,
}

impl<'id> Compiler<'id> {
    pub fn new(repo: TypeRepo<'id>, interner: &'id StringInterner) -> Self {
        Self {
            repo,
            interner,
            defined_types: vec![],
            modules: ModuleMap::default(),
            compile_queue: vec![],
            reporter: ErrorReporter::default(),
        }
    }

    pub fn run(mut self, files: &Files) -> Result<CompilationOutputs<'id>, ParseError> {
        let mut types = self.repo.type_iter().map(|id| (id.as_str().into(), id)).collect();

        let mut names = NameScope::default();
        for (name, idx) in self.repo.globals().iter_by_name() {
            names
                .top_mut()
                .entry_ref(name.name())
                .or_default()
                .push(Global::Func(idx));
        }

        let modules: Vec<_> = Self::parse_modules(files).try_collect()?;
        let mut scopes = vec![];
        for module in &modules {
            if module.path.is_empty() {
                for entry in &module.entries {
                    self.populate_entry(&module.path, entry, &mut types);
                }
                scopes.push(HashMap::default());
            } else {
                let mut local = types.introduce_scope();
                for entry in &module.entries {
                    self.populate_entry(&module.path, entry, &mut local);
                }
                scopes.push(local.pop_scope());
            };
        }

        for (module, scope) in modules.into_iter().zip(scopes) {
            let res = if module.path.is_empty() {
                self.compile_module(module, types.push_scope(scope), &mut names)
            } else {
                self.compile_module(module, types.push_scope(scope), &mut names.introduce_scope())
            };
            self.compile_queue.push(res);
        }
        self.process_inheritance();
        Ok(self.process_queue(&types, &names))
    }

    fn parse_modules(files: &Files) -> impl Iterator<Item = Result<SourceModule, ParseError>> + '_ {
        files.iter().map(|file| {
            parser::parse_file(file).map_err(|err| {
                let pos = file.byte_offset() + err.location.offset;
                ParseError(err.expected, Span::new(pos, pos))
            })
        })
    }

    fn compile_module(
        &mut self,
        module: SourceModule,
        mut types: TypeScope<'_, 'id>,
        names: &mut NameScope<'_, 'id>,
    ) -> Module<'id> {
        for import in module.imports {
            let res = self.populate_import(import, &mut types, names);
            self.reporter.unwrap_err(res);
        }
        let mut items = vec![];
        for entry in module.entries {
            let res = self.preprocess_entry(&module.path, entry, &types, names);
            let Some(item) = self.reporter.unwrap_err(res).flatten() else {
                continue;
            };
            items.push(item);
        }
        let types = types.pop_scope();
        let names = if names.is_top_level() {
            HashMap::default()
        } else {
            mem::take(names.top_mut())
        };
        Module { types, names, items }
    }

    fn populate_entry(&mut self, path: &ModulePath, entry: &SourceEntry, types: &mut TypeScope<'_, 'id>) {
        match entry {
            SourceEntry::Class(ClassSource { name, .. })
            | SourceEntry::Struct(ClassSource { name, .. })
            | SourceEntry::Enum(EnumSource { name, .. }) => {
                let type_id = generate_type_id(name, path, self.interner);
                self.modules.add_type(type_id);
                types.insert(name.clone(), type_id);
            }
            SourceEntry::Function(func) => {
                let name = ScopedName::new(func.decl.name.clone(), path.clone());
                let idx = self.repo.globals_mut().reserve_name(name.clone());
                self.modules.add_function(&name, idx);
            }
            SourceEntry::GlobalLet(_) => {}
        }
    }

    fn populate_import(
        &mut self,
        import: Import,
        types: &mut TypeScope<'_, 'id>,
        names: &mut NameScope<'_, 'id>,
    ) -> CompileResult<'id, ()> {
        match import {
            Import::Exact(_, path, span) => {
                let import = self
                    .modules
                    .get(path.iter())
                    .ok_or_else(|| CompileError::UnresolvedImport(path.into_iter().collect(), span))?;
                Self::populate_import_item(&import, &self.repo, types, names);
            }
            Import::Selected(_, path, selected, span) => {
                for name in selected {
                    let path = path.iter().chain(Some(&name));
                    let import = self
                        .modules
                        .get(path.clone())
                        .ok_or_else(|| CompileError::UnresolvedImport(path.cloned().collect(), span))?;
                    Self::populate_import_item(&import, &self.repo, types, names);
                }
            }
            Import::All(_, path, span) => {
                for descendant in self
                    .modules
                    .get_direct_descendants(path.iter())
                    .ok_or_else(|| CompileError::UnresolvedImport(path.iter().cloned().collect(), span))?
                {
                    Self::populate_import_item(&descendant, &self.repo, types, names);
                }
            }
        };
        Ok(())
    }

    fn populate_import_item(
        imported: &ImportItem<'id>,
        repo: &TypeRepo<'id>,
        types: &mut TypeScope<'_, 'id>,
        names: &mut NameScope<'_, 'id>,
    ) {
        match *imported {
            ImportItem::Type(typ) => {
                types.insert(typ.name().into(), typ);
            }
            ImportItem::Func(func) => {
                let name = repo
                    .globals()
                    .get_name(func)
                    .expect("ImportItem should point to a function");
                names
                    .top_mut()
                    .entry_ref(name.name())
                    .or_default()
                    .push(Global::Func(func));
            }
        }
    }

    fn preprocess_entry(
        &mut self,
        path: &ModulePath,
        entry: SourceEntry,
        types: &TypeScope<'_, 'id>,
        names: &mut NameScope<'_, 'id>,
    ) -> CompileResult<'id, Option<ModuleItem<'id>>> {
        let is_struct = matches!(entry, SourceEntry::Struct(_));
        match entry {
            SourceEntry::Class(class) | SourceEntry::Struct(class) => {
                let type_id = generate_type_id(&class.name, path, self.interner);
                let mut type_vars = ScopedMap::default();
                let env = TypeEnv::new(types, &type_vars);
                let class_type_vars: Box<_> = class
                    .tparams
                    .iter()
                    .map(|typ| env.instantiate_var(typ))
                    .try_collect()
                    .with_span(class.span)?;
                let mut this_args = Vec::with_capacity(class_type_vars.len());
                for var in &*class_type_vars {
                    let typ = InferType::from_var_mono(var, &type_vars);
                    type_vars.insert(var.name.clone(), typ.clone());
                    this_args.push(typ);
                }
                let extends = class
                    .base
                    .map(|base| TypeEnv::new(types, &type_vars).resolve_param_type(&base))
                    .transpose()
                    .with_span(class.span)?
                    .or_else(|| {
                        is_struct
                            .not()
                            .then(|| Parameterized::without_args(predef::ISCRIPTABLE))
                    });
                let mut data_type = ClassType {
                    type_vars: class_type_vars,
                    extends,
                    fields: FieldMap::default(),
                    methods: FuncMap::default(),
                    statics: FuncMap::default(),
                    flags: get_class_flags(&class.qualifiers).with_is_struct(is_struct),
                    span: Some(class.span),
                };
                let mut methods = vec![];
                let this = Data::new(type_id, this_args.into());

                for member in class.members {
                    match member {
                        MemberSource::Method(method) => {
                            let flags =
                                get_function_flags(&method.decl.qualifiers).with_has_body(method.body.is_some());
                            self.validate_method(data_type.flags, flags, method.decl.span);

                            let res = self.preprocess_function(&method, types, &type_vars);
                            let Some((env, typ)) = self.reporter.unwrap_err(res) else {
                                continue;
                            };
                            let index = if flags.is_static() {
                                data_type.statics.add(method.decl.name.clone(), typ, flags)
                            } else {
                                data_type.methods.add(method.decl.name.clone(), typ, flags)
                            };
                            if let Some(body) = method.body {
                                methods.push(CompileBody {
                                    name: method.decl.name,
                                    index,
                                    env,
                                    parameters: method.parameters,
                                    body,
                                    is_static: flags.is_static(),
                                });
                            }
                        }
                        MemberSource::Field(field) => {
                            let flags = get_field_flags(&field.declaration.qualifiers);
                            Self::validate_field(&mut self.reporter, data_type.flags, flags, field.declaration.span);
                            let env = TypeEnv::new(types, &type_vars);
                            let res = env.resolve_type(&field.type_).with_span(field.declaration.span);
                            let Some(typ) = self.reporter.unwrap_err(res) else {
                                continue;
                            };
                            data_type.fields.add(field.declaration.name, Field::new(typ, flags));
                        }
                    }
                }
                self.repo.add_type(type_id, DataType::Class(data_type));
                self.defined_types.push(type_id);
                Ok(Some(ModuleItem::Class(this, type_vars.pop_scope(), methods)))
            }
            SourceEntry::Enum(enum_) => {
                let type_id = generate_type_id(&enum_.name, path, self.interner);
                let members = enum_.members.iter().map(|m| (m.name.clone(), m.value)).collect();
                self.repo.add_type(type_id, DataType::Enum(EnumType { members }));
                self.defined_types.push(type_id);
                Ok(None)
            }
            SourceEntry::Function(func) => {
                let flags = get_function_flags(&func.decl.qualifiers);
                let (env, typ) = self.preprocess_function(&func, types, &ScopedMap::default())?;

                for ann in &func.decl.annotations {
                    match (&ann.kind, &ann.args[..]) {
                        (AnnotationKind::ReplaceMethod, [Expr::Ident(ident, span)]) => {
                            let span = *span;
                            let (this, entry) = self.locate_annotation_method(ident, &func.decl.name, types, span)?;
                            let body = CompileBody::new(func, entry.index, env, false)
                                .ok_or(CompileError::Unsupported(Unsupported::AnnotatedFuncWithNoBody, span))?;
                            return Ok(Some(ModuleItem::AnnotatedMethod(this, body, MethodInjection::Replace)));
                        }
                        (AnnotationKind::WrapMethod, [Expr::Ident(ident, span)]) => {
                            let span = *span;
                            let (this, entry) = self.locate_annotation_method(ident, &func.decl.name, types, span)?;
                            let body = CompileBody::new(func, entry.index, env, false)
                                .ok_or(CompileError::Unsupported(Unsupported::AnnotatedFuncWithNoBody, span))?;
                            return Ok(Some(ModuleItem::AnnotatedMethod(this, body, MethodInjection::Wrap)));
                        }
                        (AnnotationKind::AddMethod, [Expr::Ident(ident, span)]) => {
                            let span = *span;
                            let &id = types
                                .get(ident)
                                .ok_or_else(|| TypeError::UnresolvedType(ident.clone()))
                                .with_span(span)?;
                            let ct = self
                                .repo
                                .get_type_mut(id)
                                .unwrap()
                                .as_class_mut()
                                .ok_or_else(|| TypeError::UnresolvedType(ident.clone()))
                                .with_span(span)?;
                            let index = if flags.is_static() {
                                ct.statics.add(func.decl.name.clone(), typ, flags)
                            } else {
                                ct.methods.add(func.decl.name.clone(), typ, flags)
                            };
                            let body = CompileBody::new(func, index, env, false)
                                .ok_or(CompileError::Unsupported(Unsupported::AnnotatedFuncWithNoBody, span))?;
                            let data = Data::without_args(id);
                            return Ok(Some(ModuleItem::AnnotatedMethod(data, body, MethodInjection::Add)));
                        }
                        (AnnotationKind::ReplaceGlobal, []) => {
                            let span = func.decl.span;
                            let name = ScopedName::top_level(func.decl.name.clone());
                            let entry = self
                                .repo
                                .globals()
                                .by_name(&name)
                                .exactly_one()
                                .map_err(|_| CompileError::UnresolvedFunction(name.name().into(), span))?;
                            let body = CompileBody::new(func, entry.index, env, true)
                                .ok_or(CompileError::Unsupported(Unsupported::AnnotatedFuncWithNoBody, span))?;
                            return Ok(Some(ModuleItem::Global(body)));
                        }
                        (AnnotationKind::If, [_]) => {
                            todo!("conditional compilation is not supported yet")
                        }
                        _ => {
                            return Err(CompileError::Unsupported(Unsupported::InvalidAnnotation, ann.span));
                        }
                    }
                }

                let name = ScopedName::new(func.decl.name.clone(), path.clone());
                let index = self.repo.globals_mut().add(name, typ, flags);
                let global = if let Ok(intrinsic) = Intrinsic::from_str(&func.decl.name) {
                    Global::Intrinsic(index.overload(), intrinsic)
                } else {
                    Global::Func(index.overload())
                };
                let overloads = names.top_mut().entry(func.decl.name.clone()).or_default();
                if !overloads.contains(&global) {
                    overloads.push(global);
                }

                if let Some(body) = CompileBody::new(func, index, env, true) {
                    Ok(Some(ModuleItem::Global(body)))
                } else {
                    Ok(None)
                }
            }
            SourceEntry::GlobalLet(field) => {
                let span = field.declaration.span;
                let target = field
                    .declaration
                    .annotations
                    .iter()
                    .find(|ann| ann.kind == AnnotationKind::AddField)
                    .ok_or_else(|| CompileError::Unsupported(Unsupported::GlobalLetBinding, span))?;
                let [Expr::Ident(ident, ident_span)] = &target.args[..] else {
                    return Err(CompileError::Unsupported(Unsupported::InvalidAnnotation, span));
                };
                let &id = types
                    .get(ident)
                    .ok_or_else(|| TypeError::UnresolvedType(ident.clone()))
                    .with_span(*ident_span)?;
                let ct = self
                    .repo
                    .get_type_mut(id)
                    .unwrap()
                    .as_class_mut()
                    .ok_or_else(|| TypeError::UnresolvedType(ident.clone()))
                    .with_span(*ident_span)?;
                let flags = get_field_flags(&field.declaration.qualifiers);
                Self::validate_field(&mut self.reporter, ct.flags, flags, field.declaration.span);

                let res = TypeEnv::new(types, &ScopedMap::default())
                    .resolve_type(&field.type_)
                    .with_span(field.declaration.span);
                let Some(typ) = self.reporter.unwrap_err(res) else {
                    return Ok(None);
                };
                ct.fields.add(field.declaration.name, Field::new(typ, flags));
                Ok(None)
            }
        }
    }

    fn validate_method(&mut self, type_flags: ClassFlags, method_flags: FunctionFlags, span: Span) {
        if method_flags.is_native() && !type_flags.is_native() {
            self.reporter
                .report(CompileError::Unsupported(Unsupported::NativeInNonNative, span));
        }
        if !method_flags.is_static() && type_flags.is_struct() {
            self.reporter
                .report(CompileError::Unsupported(Unsupported::NonStaticStructMember, span));
        }
        if method_flags.is_final() && !method_flags.has_body() && !method_flags.is_native() {
            self.reporter
                .report(CompileError::Unsupported(Unsupported::FinalWithoutBody, span));
        }
        if method_flags.has_body() && method_flags.is_native() {
            self.reporter
                .report(CompileError::Unsupported(Unsupported::NativeWithBody, span));
        }
    }

    fn validate_field(reporter: &mut ErrorReporter<'id>, type_flags: ClassFlags, field_flags: FieldFlags, span: Span) {
        if field_flags.is_native() && !type_flags.is_native() {
            reporter.report(CompileError::Unsupported(Unsupported::NativeInNonNative, span));
        }
    }

    fn locate_annotation_target(
        &self,
        replace: &Str,
        types: &TypeScope<'_, 'id>,
        span: Span,
    ) -> CompileResult<'id, (Data<'id>, &ClassType<'id>)> {
        let &id = types
            .get(replace)
            .ok_or_else(|| TypeError::UnresolvedType(replace.clone()))
            .with_span(span)?;
        let res = self.repo[id]
            .as_class()
            .ok_or_else(|| TypeError::UnresolvedType(replace.clone()))
            .with_span(span)?;
        Ok((Data::without_args(id), res))
    }

    fn locate_annotation_method(
        &self,
        replace: &Str,
        name: &Str,
        types: &TypeScope<'_, 'id>,
        span: Span,
    ) -> CompileResult<'id, (Data<'id>, OverloadEntry<'_, 'id>)> {
        let (data, res) = self.locate_annotation_target(replace, types, span)?;
        let entry = res
            .methods
            .by_name(name)
            .exactly_one()
            .map_err(|_| CompileError::UnresolvedFunction(name.clone(), span))?;
        Ok((data, entry))
    }

    fn process_queue(mut self, types: &TypeScope<'_, 'id>, names: &NameScope<'_, 'id>) -> CompilationOutputs<'id> {
        let mut items = vec![];

        for module in self.compile_queue {
            let types = types.push_scope(module.types);
            let names = names.push_scope(module.names);
            for item in module.items {
                match item {
                    ModuleItem::Class(this, env, funcs) => {
                        let type_vars = ScopedMap::Tail(env);
                        for func in funcs {
                            let CompileBody { index, is_static, .. } = func;
                            let mid = MethodId::new(this.id, index);
                            let method = if is_static {
                                self.repo.get_static(&mid).unwrap()
                            } else {
                                self.repo.get_method(&mid).unwrap()
                            };
                            let this = is_static.not().then(|| InferType::data(this.clone()));
                            let (body, params) = Self::compile_function(
                                func,
                                &method.typ,
                                &self.repo,
                                &types,
                                &names,
                                &type_vars,
                                this,
                                &mut self.reporter,
                            );
                            items.push(CodeGenItem::AssembleMethod(mid, params, body, is_static));
                        }
                    }
                    ModuleItem::Global(body) => {
                        let idx = body.index;
                        let func = self.repo.get_global(&GlobalId::new(body.index)).unwrap();
                        let type_vars = ScopedMap::default();
                        let (body, params) = Self::compile_function(
                            body,
                            &func.typ,
                            &self.repo,
                            &types,
                            &names,
                            &type_vars,
                            None,
                            &mut self.reporter,
                        );
                        items.push(CodeGenItem::AssembleGlobal(GlobalId::new(idx), params, body));
                    }
                    ModuleItem::AnnotatedMethod(this, body, kind) => {
                        let CompileBody { index, is_static, .. } = body;
                        let mid = MethodId::new(this.id, index);
                        let method = if is_static {
                            self.repo.get_static(&mid).unwrap()
                        } else {
                            self.repo.get_method(&mid).unwrap()
                        };
                        let this = is_static.not().then(|| InferType::data(this.clone()));
                        let mut names = names.introduce_scope();
                        if kind == MethodInjection::Wrap {
                            let alias = if is_static {
                                Global::StaticAlias(mid.clone())
                            } else {
                                Global::MethodAlias(mid.clone())
                            };
                            names.insert(Str::from_static("wrappedMethod"), vec![alias]);
                        }
                        let (body, params) = Self::compile_function(
                            body,
                            &method.typ,
                            &self.repo,
                            &types,
                            &names,
                            &ScopedMap::default(),
                            this,
                            &mut self.reporter,
                        );
                        match kind {
                            MethodInjection::Add => {
                                items.push(CodeGenItem::AddMethod(mid, params, body, is_static));
                            }
                            MethodInjection::Replace => {
                                items.push(CodeGenItem::AssembleMethod(mid, params, body, is_static));
                            }
                            MethodInjection::Wrap => {
                                items.push(CodeGenItem::WrapMethod(mid, params, body, is_static));
                            }
                        }
                    }
                }
            }
        }
        CompilationOutputs {
            repo: self.repo,
            defined_types: self.defined_types,
            codegen_queue: items,
            reporter: self.reporter,
        }
    }

    fn preprocess_function(
        &mut self,
        func: &FunctionSource,
        types: &TypeScope<'_, 'id>,
        vars: &Vars<'_, 'id>,
    ) -> CompileResult<'id, (HashMap<Str, InferType<'id>>, FuncType<'id>)> {
        let env = TypeEnv::new(types, vars);
        let method_type_vars = func
            .tparams
            .iter()
            .map(|ty| env.instantiate_var(ty))
            .collect::<CompileResult<'_, Box<[_]>, _>>()
            .with_span(func.decl.span)?;
        let mut local_vars = vars.introduce_scope();
        for var in &*method_type_vars {
            local_vars.insert(var.name.clone(), InferType::from_var_mono(var, &local_vars));
        }
        let env = env.with_vars(&local_vars);
        let params = func
            .parameters
            .iter()
            .map(|param| {
                let typ = env.resolve_type(&param.type_)?;
                Ok(FuncParam::custom(typ, param.qualifiers.contain(Qualifier::Out)))
            })
            .try_collect()
            .with_span(func.decl.span)?;
        let ret = func
            .type_
            .as_ref()
            .map(|typ| env.resolve_type(typ))
            .unwrap_or(Ok(Type::Prim(Prim::Void)))
            .with_span(func.decl.span)?;
        let func_type = FuncType::new(method_type_vars, params, ret.clone());
        Ok((local_vars.pop_scope(), func_type))
    }

    #[allow(clippy::too_many_arguments)]
    fn compile_function(
        mut body: CompileBody<'id>,
        typ: &FuncType<'id>,
        repo: &TypeRepo<'id>,
        types: &TypeScope<'_, 'id>,
        names: &NameScope<'_, 'id>,
        vars: &Vars<'_, 'id>,
        this: Option<InferType<'id>>,
        reporter: &mut ErrorReporter<'id>,
    ) -> (Seq<CheckedAst<'id>>, IndexMap<Local, Type<'id>>) {
        let local_vars = vars.push_scope(body.env);
        let mut id_alloc = IdAlloc::default();
        let mut locals = ScopedMap::default();
        let mut params = IndexMap::default();
        let mut boxed = ScopedMap::default();

        for (lhs, rhs) in body.parameters.iter().zip(typ.params.iter()) {
            let info = id_alloc.allocate_param(InferType::from_type(&rhs.typ, &local_vars));
            if rhs.is_poly {
                if let Some(boxable) = Boxable::from_infer_type(&info.typ, repo) {
                    boxed.insert(info.local, boxable);
                }
            }
            params.insert(info.local, rhs.typ.clone());
            locals.insert(lhs.name.clone(), info);
        }
        if let Some(this) = this {
            locals.insert(Str::from_static("this"), LocalInfo::new(Local::This, this));
        }
        let ret = InferType::from_type(&typ.ret, &local_vars);
        let poly_ret = typ.is_ret_poly.then(|| Boxable::from_infer_type(&ret, repo)).flatten();
        let env = TypeEnv::new(types, &local_vars);

        Desugar::run(&mut body.body);
        let mut id_alloc = IdAlloc::default();
        let mut seq = Typer::run(repo, names, env, &body.body, &mut locals, ret, &mut id_alloc, reporter);
        Autobox::run(&mut seq, repo, boxed, poly_ret);
        (seq, params)
    }

    fn process_inheritance(&mut self) {
        // resolve all base methods by type signatures
        let mut method_to_base = HashMap::new();

        for module in &self.compile_queue {
            for item in &module.items {
                let ModuleItem::Class(this, _, funcs) = item else {
                    continue;
                };
                let Some(base) = self.repo[this.id]
                    .as_class()
                    .and_then(|class| class.extends.as_ref())
                    .and_then(|typ| self.repo[typ.id].as_class())
                else {
                    continue;
                };
                if let Some(span) = base.span.filter(|_| base.flags.is_final()) {
                    self.reporter
                        .report(CompileError::Unsupported(Unsupported::ExtendingFinalClass, span));
                }

                for func in funcs {
                    let &CompileBody { index, is_static, .. } = func;
                    if is_static {
                        continue;
                    }
                    let mid = MethodId::new(this.id, index);
                    let method = self.repo.get_method(&mid).unwrap();
                    let Some(base) = Self::get_base_method(this.id, &func.name, this, &method.typ, &self.repo) else {
                        continue;
                    };
                    method_to_base.insert(mid, base);
                }
            }
        }

        // sort classes by the number of types they extend
        self.defined_types.sort_by_key(|typ| self.repo.upper_iter(*typ).count());

        let mut unimplemented: HashMap<TypeId<'id>, HashSet<MethodId<'id>>> = HashMap::new();

        // resolve all unimplemented virtual methods
        for &typ in &self.defined_types {
            let DataType::Class(class) = &self.repo[typ] else {
                continue;
            };
            let mut this_unimplemented = class
                .extends
                .as_ref()
                .and_then(|base| unimplemented.get(&base.id))
                .cloned()
                .unwrap_or_default();

            for entry in class.methods.iter() {
                let mid = MethodId::new(typ, entry.index);
                if !entry.function.is_implemented() {
                    this_unimplemented.insert(mid);
                } else if let Some(base) = method_to_base.get(&mid) {
                    this_unimplemented.remove(base);
                }
            }

            if !class.flags.is_abstract() && !this_unimplemented.is_empty() {
                for method in &this_unimplemented {
                    let name = self.repo.get_method_name(method).unwrap();
                    let span = class.span.expect("span should be defined on user classes");
                    self.reporter
                        .report(CompileError::UnimplementedMethod(name.clone(), span));
                }
            }

            unimplemented.insert(typ, this_unimplemented);
        }

        // promote parameters with overriden generic parameters into polymorphic ones
        for (mid, base_id) in &method_to_base {
            let mut root = base_id;
            while let Some(id) = method_to_base.get(root) {
                root = id;
            }
            let [method, base] = self.repo.get_many_method_mut([mid, root]).unwrap();
            method.typ.is_ret_poly = matches!(base.typ.ret, Type::Var(_)) && !matches!(method.typ.ret, Type::Var(_));
            method.base = Some(base_id.clone());

            for (l, r) in base.typ.params.iter().zip(method.typ.params.iter_mut()) {
                if matches!(l.typ, Type::Var(_)) && !matches!(r.typ, Type::Var(_)) {
                    r.is_poly = true;
                }
            }
        }
    }

    fn get_base_method(
        owner: TypeId<'id>,
        name: &str,
        this: &Data<'id>,
        typ: &FuncType<'id>,
        repo: &TypeRepo<'id>,
    ) -> Option<MethodId<'id>> {
        let (id, entry) = repo
            .upper_iter(owner)
            .skip(1)
            .flat_map(|(type_id, class)| class.methods.by_name(name).map(move |res| (type_id, res)))
            .filter(|(_, e)| e.function.typ.params.len() == typ.params.len() && !e.function.flags.is_final())
            .filter(|(id, e)| {
                let base = this
                    .clone()
                    .instantiate_as(*id, repo)
                    .expect("should always match upper bound type");
                let vars = repo[*id].type_var_names().zip(base.args.iter().cloned()).collect();
                e.function
                    .typ
                    .params
                    .iter()
                    .map(|param| InferType::from_type_with(&param.typ, &vars, true))
                    .zip(typ.params.iter())
                    .all(|(l, r)| l.is_same_shape(&r.typ))
            })
            .at_most_one()
            .ok()
            .expect("there should no more than one base method")?;
        Some(MethodId::new(id, entry.index))
    }
}

#[derive(Debug)]
pub struct CompilationOutputs<'id> {
    repo: TypeRepo<'id>,
    defined_types: Vec<TypeId<'id>>,
    codegen_queue: Vec<CodeGenItem<'id>>,
    reporter: ErrorReporter<'id>,
}

impl<'id> CompilationOutputs<'id> {
    pub fn commit(self, db: &mut CompilationDb<'id>, cache: &mut TypeCache, pool: &mut ConstantPool) {
        for &item in &self.defined_types {
            match self.repo[item] {
                DataType::Class(_) => {
                    db.classes.insert(item, pool.reserve());
                }
                DataType::Enum(_) => {
                    db.enums.insert(item, pool.reserve());
                }
                _ => {}
            }
        }

        for item in self.defined_types {
            Self::build_type(item, &self.repo, db, cache, pool);
        }

        let mut wrappers: HashMap<MethodId<'id>, VecDeque<_>> = HashMap::new();

        for (i, item) in self.codegen_queue.iter().enumerate() {
            match item {
                &CodeGenItem::AssembleGlobal(id, _, _) => {
                    let (sig, method) = self.repo.globals().get_overload(id.into()).unwrap();
                    let flags = method.flags.with_is_static(true).with_is_final(true);
                    let idx = Self::build_function(sig.clone(), &method.typ, flags, None, db)
                        .commit_global(&self.repo, pool, cache);
                    db.globals.insert(id, idx);
                }
                CodeGenItem::AddMethod(mid, _, _, is_static) => {
                    let (sig, method) = if *is_static {
                        self.repo.get_static_with_signature(mid).unwrap()
                    } else {
                        self.repo.get_method_with_signature(mid).unwrap()
                    };
                    let &parent = db.classes.get(&mid.owner()).unwrap();
                    let idx = Self::build_function(sig.clone(), &method.typ, method.flags, method.base.as_ref(), db)
                        .commit(parent, &self.repo, pool, cache);

                    pool[parent].methods.push(idx);
                    db.methods.insert(mid.clone(), idx);
                }
                CodeGenItem::WrapMethod(mid, _, _, is_static) => {
                    let (sig, method) = if *is_static {
                        self.repo.get_static_with_signature(mid).unwrap()
                    } else {
                        self.repo.get_method_with_signature(mid).unwrap()
                    };
                    let &parent = db.classes.get(&mid.owner()).unwrap();
                    let wrapper_sig = FuncSignature::new(names::wrapper(i, sig.clone().into_str()));
                    let idx = Self::build_function(wrapper_sig, &method.typ, method.flags, method.base.as_ref(), db)
                        .with_wrapper_flag()
                        .commit(parent, &self.repo, pool, cache);

                    pool[parent].methods.push(idx);
                    wrappers.entry(mid.clone()).or_default().push_back(idx);
                }
                _ => {}
            }
        }

        for (mid, indexes) in &mut wrappers {
            let wrapped_idx = *db.methods.get(mid).unwrap();
            let last_wrapper_idx = indexes.pop_back().expect("should have at least one wrapper");

            let wrapped_name = pool.def_name_idx(wrapped_idx).unwrap();
            let wrapper_name = pool.def_name_idx(last_wrapper_idx).unwrap();

            let wrapped = &mut pool[wrapped_idx];
            if wrapped.flags.is_callback() {
                // make sure only one remains a callback
                wrapped.flags = wrapped.flags.with_is_callback(false);
                pool[last_wrapper_idx].flags = wrapped.flags.with_is_callback(true);
            }

            // the game crashes when parameter names are not aligned sometimes
            let from_params = pool[wrapped_idx].parameters.clone();
            let to_params = pool[last_wrapper_idx].parameters.clone();
            for (&from, &to) in from_params.iter().zip(&to_params) {
                pool.rename(to, pool.def_name_idx(from).unwrap());
            }

            Self::remap_locals(last_wrapper_idx, wrapped_idx, pool);
            pool.rename(last_wrapper_idx, wrapped_name);
            pool.rename(wrapped_idx, wrapper_name);
            pool.swap_definition(wrapped_idx, last_wrapper_idx);

            indexes.push_front(last_wrapper_idx);
            indexes.push_back(wrapped_idx);
        }

        for item in self.codegen_queue {
            match item {
                CodeGenItem::AssembleMethod(mid, params, body, is_static)
                | CodeGenItem::AddMethod(mid, params, body, is_static) => {
                    let &idx = if is_static {
                        db.statics.get(&mid).unwrap()
                    } else {
                        db.methods.get(&mid).unwrap()
                    };
                    let param_indices = LocalIndices::new(params, pool[idx].parameters.iter().copied().collect());
                    let (locals, code) =
                        CodeGen::build_function(body, param_indices, &self.repo, db, None, pool, cache);
                    pool.complete_function(idx, locals.into_vec(), code);
                }
                CodeGenItem::AssembleGlobal(gid, params, body) => {
                    let &idx = db.globals.get(&gid).unwrap();
                    let param_indices = LocalIndices::new(params, pool[idx].parameters.iter().copied().collect());
                    let (locals, code) =
                        CodeGen::build_function(body, param_indices, &self.repo, db, None, pool, cache);
                    pool.complete_function(idx, locals.into_vec(), code);
                }
                CodeGenItem::WrapMethod(mid, params, body, _) => {
                    let indexes = wrappers.get_mut(&mid).expect("wrapper should have been created");
                    let wrapped = indexes.pop_front().expect("should have at least one wrapped method");
                    let index = indexes.front().copied().expect("should have at least one wrapper");

                    let param_indices = LocalIndices::new(params, pool[index].parameters.iter().copied().collect());
                    let (locals, code) =
                        CodeGen::build_function(body, param_indices, &self.repo, db, Some(wrapped), pool, cache);
                    pool.complete_function(index, locals.into_vec(), code);
                }
            }
        }
    }

    fn build_type(
        id: TypeId<'id>,
        repo: &TypeRepo<'id>,
        db: &mut CompilationDb<'id>,
        cache: &mut TypeCache,
        pool: &mut ConstantPool,
    ) {
        match &repo[id] {
            DataType::Class(class_type) => {
                let &class_idx = db.classes.get(&id).unwrap();
                let base = class_type.extends.as_ref().and_then(|c| db.classes.get(&c.id)).copied();
                let fields = class_type.fields.iter().map(|entry| {
                    FieldBuilder::builder()
                        .name(entry.name.clone())
                        .typ(entry.field.typ.clone())
                        .flags(entry.field.flags)
                        .build()
                });
                let methods = chain!(
                    class_type.statics.iter().map(|e| Self::build_function(
                        e.signature.clone(),
                        &e.function.typ,
                        e.function.flags,
                        None,
                        db
                    )),
                    class_type.methods.iter().map(|e| Self::build_function(
                        e.signature.clone(),
                        &e.function.typ,
                        e.function.flags,
                        e.function.base.as_ref(),
                        db
                    ))
                );
                let idx = ClassBuilder::builder()
                    .name(id.as_str())
                    .fields(fields)
                    .methods(methods)
                    .flags(class_type.flags)
                    .build()
                    .commit_as(class_idx, base.unwrap_or(PoolIndex::UNDEFINED), repo, pool, cache);

                let class = &pool[idx];
                for (entry, &idx) in class_type.fields.iter().zip(&class.fields) {
                    db.fields.insert(FieldId::new(id, entry.index), idx);
                }
                let mut ms = class.methods.iter();
                for (entry, &idx) in class_type.statics.iter().zip(ms.by_ref()) {
                    db.statics.insert(MethodId::new(id, entry.index), idx);
                }
                for (entry, &idx) in class_type.methods.iter().zip(ms.by_ref()) {
                    db.methods.insert(MethodId::new(id, entry.index), idx);
                }
            }
            DataType::Enum(typ) => {
                let &enum_idx = db.enums.get(&id).unwrap();
                let idx = EnumBuilder::builder()
                    .name(id.as_str())
                    .members(typ.iter().map(|e| (e.name.clone(), e.value)))
                    .build()
                    .commit_as(pool, enum_idx);
                for (entry, &member) in typ.iter().zip(&pool[idx].members) {
                    db.enum_members.insert(FieldId::new(id, entry.index), member);
                }
            }
            DataType::Builtin { .. } => {}
        }
    }

    fn build_function(
        signature: FuncSignature,
        typ: &FuncType<'id>,
        flags: FunctionFlags,
        base: Option<&MethodId<'id>>,
        db: &CompilationDb<'id>,
    ) -> FunctionBuilder<'id> {
        let params = typ.params.iter().enumerate().map(|(i, param)| {
            ParamBuilder::builder()
                .name(names::param(i))
                .typ(if param.is_poly { Type::Top } else { param.typ.clone() })
                .flags(ParameterFlags::new().with_is_out(param.is_out))
                .build()
        });
        let base_idx = base.map(|mid| *db.methods.get(mid).unwrap());

        FunctionBuilder::builder()
            .flags(flags)
            .visibility(Visibility::Public)
            .name(signature.into_str())
            .return_type(if typ.is_ret_poly { Type::Top } else { typ.ret.clone() })
            .params(params)
            .base(base_idx)
            .build()
    }

    fn remap_locals(proxy: PoolIndex<PoolFunction>, target: PoolIndex<PoolFunction>, pool: &mut ConstantPool) {
        // this is a workaround for a game crash which happens when the game loads
        // locals that are not placed adjacent to the parent function in the pool
        let locals = pool[target].locals.clone();
        let mut mapped_locals = HashMap::new();
        for local_idx in locals {
            let mut local = pool.definition(local_idx).unwrap().clone();
            local.parent = proxy.cast();
            mapped_locals.insert(local_idx, pool.add_definition(local));
        }

        let fun = &mut pool[target];
        fun.locals = mapped_locals.values().copied().collect();

        for instr in &mut fun.code.0 {
            if let Instr::Local(local) = instr {
                *instr = Instr::Local(*mapped_locals.get(local).expect("mapped local should exist"));
            }
        }
    }

    pub fn reporter(&self) -> &ErrorReporter<'id> {
        &self.reporter
    }

    pub fn into_errors(self) -> Vec<CompileError<'id>> {
        self.reporter.into_errors()
    }
}

#[derive(Debug, Default)]
pub struct CompilationDb<'id> {
    pub(crate) classes: HashMap<TypeId<'id>, PoolIndex<PoolClass>>,
    pub(crate) fields: HashMap<FieldId<'id>, PoolIndex<PoolField>>,
    pub(crate) methods: HashMap<MethodId<'id>, PoolIndex<PoolFunction>>,
    pub(crate) statics: HashMap<MethodId<'id>, PoolIndex<PoolFunction>>,
    pub(crate) globals: HashMap<GlobalId, PoolIndex<PoolFunction>>,
    pub(crate) enums: HashMap<TypeId<'id>, PoolIndex<PoolEnum>>,
    pub(crate) enum_members: HashMap<FieldId<'id>, PoolIndex<i64>>,
}

impl<'id> CompilationDb<'id> {
    fn load_class(
        &mut self,
        owner: TypeId<'id>,
        idx: PoolIndex<PoolClass>,
        pool: &ConstantPool,
        interner: &'id StringInterner,
    ) -> ClassType<'id> {
        self.classes.insert(owner, idx);
        let class = &pool[idx];
        let mut fields = FieldMap::default();
        for &idx in &class.fields {
            let name = pool.def_name(idx).unwrap();
            let field = &pool[idx];
            let typ = CompilationDb::load_type(field.type_, pool, interner);
            let index = fields.add(name.into(), Field::new(typ, field.flags));
            self.fields.insert(FieldId::new(owner, index), idx);
        }

        let mut methods = FuncMap::default();
        let mut statics = FuncMap::default();
        for &idx in &class.methods {
            let method = &pool[idx];
            let (short_name, signature, ftyp) = CompilationDb::load_function(idx, pool, interner);
            if method.flags.is_static() {
                let index = statics.add_with_signature(short_name, signature, ftyp, method.flags);
                self.statics.insert(MethodId::new(owner, index), idx);
            } else {
                let index = methods.add_with_signature(short_name, signature, ftyp, method.flags);
                self.methods.insert(MethodId::new(owner, index), idx);
            }
        }

        let base = class.base.is_undefined().not().then(|| {
            let name = pool.def_name(class.base).unwrap();
            Parameterized::without_args(get_type_id(name, interner))
        });

        ClassType {
            type_vars: [].into(),
            extends: base,
            fields,
            methods,
            statics,
            flags: class.flags,
            span: None,
        }
    }

    fn load_enum(&mut self, owner: TypeId<'id>, idx: PoolIndex<PoolEnum>, pool: &ConstantPool) -> EnumType {
        self.enums.insert(owner, idx);
        let mut typ = EnumType::default();
        for &idx in &pool[idx].members {
            let name = pool.def_name(idx).unwrap();
            let i = typ.add_member(name.into(), pool[idx]);
            self.enum_members.insert(FieldId::new(owner, i), idx);
        }
        typ
    }

    fn load_function(
        idx: PoolIndex<PoolFunction>,
        pool: &ConstantPool,
        interner: &'id StringInterner,
    ) -> (Str, FuncSignature, FuncType<'id>) {
        let name = pool.def_name(idx).unwrap();
        let func = &pool[idx];
        let ret = func
            .return_type
            .map_or(Type::Prim(Prim::Void), |idx| Self::load_type(idx, pool, interner));
        let params = func
            .parameters
            .iter()
            .map(|&idx| {
                let param = &pool[idx];
                let typ = Self::load_type(param.type_, pool, interner);
                FuncParam::custom(typ, param.flags.is_out())
            })
            .collect();
        let short_name = name.split_once(';').map_or_else(|| name, |(s, _)| s);
        let ftyp = FuncType::new([].into(), params, ret);
        (short_name.into(), FuncSignature::new(name.into()), ftyp)
    }

    fn load_type(idx: PoolIndex<PoolType>, pool: &ConstantPool, interner: &'id StringInterner) -> Type<'id> {
        match &pool[idx] {
            PoolType::Prim => {
                let str = pool.def_name(idx).unwrap();
                Type::Prim(Prim::from_str(str).expect("should be a known primitive type"))
            }
            PoolType::Class => {
                let name = pool.def_name(idx).unwrap();
                Type::Data(Parameterized::new(get_type_id(name, interner), Rc::new([])))
            }
            &PoolType::Ref(inner) => Self::load_type(inner, pool, interner),
            &PoolType::WeakRef(inner) => {
                let inner = Self::load_type(inner, pool, interner);
                Type::Data(Parameterized::new(predef::WREF, Rc::new([inner])))
            }
            &PoolType::ScriptRef(inner) => {
                let inner = Self::load_type(inner, pool, interner);
                Type::Data(Parameterized::new(predef::SCRIPT_REF, Rc::new([inner])))
            }
            &PoolType::Array(inner) | &PoolType::StaticArray(inner, _) => {
                let inner = Self::load_type(inner, pool, interner);
                Type::Data(Parameterized::new(predef::ARRAY, Rc::new([inner])))
            }
        }
    }
}

#[derive(Debug)]
pub struct CompilationResources<'id> {
    pub type_repo: TypeRepo<'id>,
    pub type_cache: TypeCache,
    pub db: CompilationDb<'id>,
}

impl<'id> CompilationResources<'id> {
    pub fn load(pool: &ConstantPool, interner: &'id StringInterner) -> Self {
        let mut type_repo = TypeRepo::default();
        let mut type_cache = TypeCache::default();
        let mut db = CompilationDb::default();

        for (idx, def) in pool.definitions() {
            match &def.value {
                AnyDefinition::Type(_) => {
                    let mangled = pool.names()[def.name].into();
                    type_cache.add(mangled, idx.cast());
                }
                AnyDefinition::Class(_) => {
                    let name = &pool.names()[def.name];
                    let owner = get_type_id(name, interner);
                    let class = db.load_class(owner, idx.cast(), pool, interner);
                    type_repo.add_type(owner, DataType::Class(class));
                }
                AnyDefinition::Function(fun) if def.parent.is_undefined() => {
                    let (name, sig, ftyp) = CompilationDb::load_function(idx.cast(), pool, interner);
                    let id =
                        type_repo
                            .globals_mut()
                            .add_with_signature(ScopedName::top_level(name), sig, ftyp, fun.flags);
                    db.globals.insert(GlobalId::new(id), idx.cast());
                }
                AnyDefinition::Enum(_) => {
                    let name = &pool.names()[def.name];
                    let owner = get_type_id(name, interner);
                    let enum_ = db.load_enum(owner, idx.cast(), pool);
                    type_repo.add_type(owner, DataType::Enum(enum_));
                }
                _ => {}
            }
        }
        Self {
            type_repo,
            type_cache,
            db,
        }
    }
}

fn generate_type_id<'id>(name: &Str, path: &ModulePath, interner: &'id StringInterner) -> TypeId<'id> {
    if path.is_empty() {
        return get_type_id(name, interner);
    }
    let str = path.iter().chain(Some(name)).join(".");
    TypeId::from_interned(interner.intern(str))
}

fn get_type_id<'id>(name: &str, interner: &'id StringInterner) -> TypeId<'id> {
    TypeId::get_predefined_by_name(name).unwrap_or_else(|| TypeId::from_interned(interner.intern(name)))
}

#[derive(Debug)]
struct Module<'id> {
    names: HashMap<Str, Vec<Global<'id>>>,
    types: HashMap<Str, TypeId<'id>>,
    items: Vec<ModuleItem<'id>>,
}

#[derive(Debug)]
enum ModuleItem<'id> {
    Class(Data<'id>, HashMap<Str, InferType<'id>>, Vec<CompileBody<'id>>),
    Global(CompileBody<'id>),
    AnnotatedMethod(Data<'id>, CompileBody<'id>, MethodInjection),
}

#[derive(Debug, PartialEq, Eq)]
enum MethodInjection {
    Replace,
    Add,
    Wrap,
}

#[derive(Debug)]
struct CompileBody<'id> {
    name: Str,
    index: OverloadIndex,
    env: HashMap<Str, InferType<'id>>,
    parameters: Vec<ParameterSource>,
    body: Seq<SourceAst>,
    is_static: bool,
}

impl<'id> CompileBody<'id> {
    pub fn new(
        func: FunctionSource,
        index: OverloadIndex,
        env: HashMap<Str, InferType<'id>>,
        is_global: bool,
    ) -> Option<Self> {
        let res = CompileBody {
            name: func.decl.name.clone(),
            index,
            env,
            parameters: func.parameters,
            body: func.body?,
            is_static: is_global || func.decl.qualifiers.contain(Qualifier::Static),
        };
        Some(res)
    }
}

#[derive(Debug)]
enum CodeGenItem<'id> {
    AddMethod(MethodId<'id>, IndexMap<Local, Type<'id>>, Seq<CheckedAst<'id>>, bool),
    WrapMethod(MethodId<'id>, IndexMap<Local, Type<'id>>, Seq<CheckedAst<'id>>, bool),
    AssembleMethod(MethodId<'id>, IndexMap<Local, Type<'id>>, Seq<CheckedAst<'id>>, bool),
    AssembleGlobal(GlobalId, IndexMap<Local, Type<'id>>, Seq<CheckedAst<'id>>),
}

#[derive(Debug, Clone)]
enum ImportItem<'id> {
    Type(TypeId<'id>),
    Func(FuncIndex),
}

#[derive(Debug, Default)]
struct ModuleMap<'id> {
    map: SequenceTrie<Str, ImportItem<'id>, RandomState>,
}

impl<'id> ModuleMap<'id> {
    pub fn get_direct_descendants<'this>(
        &'this self,
        path: impl IntoIterator<Item = &'this Str> + 'this,
    ) -> Option<impl Iterator<Item = ImportItem<'id>> + 'this> {
        let node = self.map.get_node(path)?;
        Some(node.children().filter_map(SequenceTrie::value).cloned())
    }

    #[inline]
    pub fn get<'this>(&'this self, path: impl IntoIterator<Item = &'this Str> + 'this) -> Option<ImportItem<'id>> {
        self.map.get(path).cloned()
    }

    pub fn add_function(&mut self, name: &ScopedName, f: FuncIndex) {
        self.map.insert_owned(name.as_parts().cloned(), ImportItem::Func(f));
    }

    pub fn add_type(&mut self, typ: TypeId<'id>) {
        self.map
            .insert_owned(typ.as_parts().map(Str::from), ImportItem::Type(typ));
    }
}

fn get_function_flags(qualifiers: &Qualifiers) -> FunctionFlags {
    let is_static = qualifiers.contain(Qualifier::Static);
    FunctionFlags::new()
        .with_is_native(qualifiers.contain(Qualifier::Native))
        .with_is_callback(qualifiers.contain(Qualifier::Callback))
        .with_is_final(is_static || qualifiers.contain(Qualifier::Final))
        .with_is_quest(qualifiers.contain(Qualifier::Quest))
        .with_is_static(is_static)
}

fn get_class_flags(qualifiers: &Qualifiers) -> ClassFlags {
    let is_import_only = qualifiers.contain(Qualifier::ImportOnly);
    ClassFlags::new()
        .with_is_native(is_import_only || qualifiers.contain(Qualifier::Native))
        .with_is_import_only(is_import_only)
        .with_is_abstract(qualifiers.contain(Qualifier::Abstract))
        .with_is_final(qualifiers.contain(Qualifier::Final))
}

fn get_field_flags(qualifiers: &Qualifiers) -> FieldFlags {
    FieldFlags::new()
        .with_is_native(qualifiers.contain(Qualifier::Native))
        .with_is_persistent(qualifiers.contain(Qualifier::Persistent))
}
