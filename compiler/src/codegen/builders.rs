use std::ops::{Deref, Not};
use std::rc::Rc;

use hashbrown::HashMap;
use itertools::{Either, Itertools};
use redscript::bundle::{ConstantPool, PoolIndex};
use redscript::bytecode::{Code, Offset};
use redscript::definition::{
    Class, ClassFlags, Definition, Enum, Field, FieldFlags, Function, FunctionFlags, Local, LocalFlags, Parameter,
    ParameterFlags, SourceReference, Type as PoolType, Visibility,
};
use redscript::{str_fmt, Str};
use typed_builder::TypedBuilder;

use crate::type_repo::{predef, DataType, Parameterized, Prim, Type, TypeRepo};

#[derive(Debug, TypedBuilder)]
pub struct ClassBuilder<'id> {
    #[builder(default = ClassFlags::new())]
    flags: ClassFlags,
    #[builder(default = Visibility::Private)]
    visibility: Visibility,
    #[builder(setter(into))]
    name: Str,
    #[builder(default, setter(transform = |it: impl IntoIterator<Item=FunctionBuilder<'id>>| it.into_iter().collect()))]
    methods: Vec<FunctionBuilder<'id>>,
    #[builder(default, setter(transform = |it: impl IntoIterator<Item=FieldBuilder<'id>>| it.into_iter().collect()))]
    fields: Vec<FieldBuilder<'id>>,
}

impl<'id> ClassBuilder<'id> {
    #[inline]
    pub fn commit(self, repo: &TypeRepo<'id>, pool: &mut ConstantPool, cache: &mut TypeCache) -> PoolIndex<Class> {
        self.commit_with_base(PoolIndex::UNDEFINED, repo, pool, cache)
    }

    #[inline]
    pub fn commit_with_base(
        self,
        base: PoolIndex<Class>,
        repo: &TypeRepo<'id>,
        pool: &mut ConstantPool,
        cache: &mut TypeCache,
    ) -> PoolIndex<Class> {
        self.commit_as(pool.reserve(), base, repo, pool, cache)
    }

    pub fn commit_as(
        self,
        id: PoolIndex<Class>,
        base: PoolIndex<Class>,
        repo: &TypeRepo<'id>,
        pool: &mut ConstantPool,
        cache: &mut TypeCache,
    ) -> PoolIndex<Class> {
        let name = pool.names.add(self.name);
        let methods = self
            .methods
            .into_iter()
            .map(|method| method.commit(id, repo, pool, cache))
            .collect();
        let fields = self
            .fields
            .into_iter()
            .map(|field| field.commit(id, repo, pool, cache))
            .collect();
        let def = Class {
            visibility: self.visibility,
            flags: self.flags,
            base,
            methods,
            fields,
            overrides: vec![],
        };
        pool.put_definition(id, Definition::class(name, def));
        id
    }
}

#[derive(Debug, TypedBuilder)]
pub struct EnumBuilder {
    #[builder(setter(into))]
    name: Str,
    #[builder(default, setter(transform = |it: impl IntoIterator<Item=(Str, i64)>| it.into_iter().collect()))]
    members: Vec<(Str, i64)>,
}

impl EnumBuilder {
    pub fn commit_as(self, pool: &mut ConstantPool, idx: PoolIndex<Enum>) -> PoolIndex<Enum> {
        let name = pool.names.add(self.name);
        let members = self
            .members
            .into_iter()
            .map(|(name, val)| {
                let name = pool.names.add(name);
                pool.add_definition(Definition::enum_value(name, idx, val))
            })
            .collect_vec();
        let def = Enum {
            flags: 0,
            size: members.len() as u8,
            members,
            unk1: false,
        };
        pool.put_definition(idx, Definition::enum_(name, def));
        idx
    }
}

#[derive(Debug, TypedBuilder)]
pub struct FunctionBuilder<'id> {
    #[builder(default = FunctionFlags::new())]
    flags: FunctionFlags,
    #[builder(default = Visibility::Private)]
    visibility: Visibility,
    #[builder(setter(into))]
    name: Str,
    #[builder(default = Type::Prim(Prim::Unit))]
    return_type: Type<'id>,
    #[builder(default, setter(transform = |it: impl IntoIterator<Item=ParamBuilder<'id>>| it.into_iter().collect()))]
    params: Vec<ParamBuilder<'id>>,
    #[builder(default, setter(transform = |it: impl IntoIterator<Item=LocalBuilder<'id>>| it.into_iter().collect()))]
    locals: Vec<LocalBuilder<'id>>,
    #[builder(default = Code::EMPTY)]
    body: Code<Offset>,
    #[builder(default = None)]
    base: Option<PoolIndex<Function>>,
    #[builder(default = false)]
    is_wrapper: bool,
}

impl<'id> FunctionBuilder<'id> {
    pub fn commit_global(
        self,
        repo: &TypeRepo<'id>,
        pool: &mut ConstantPool,
        cache: &mut TypeCache,
    ) -> PoolIndex<Function> {
        self.commit(PoolIndex::UNDEFINED, repo, pool, cache)
    }

    pub fn commit(
        self,
        parent: PoolIndex<Class>,
        repo: &TypeRepo<'id>,
        pool: &mut ConstantPool,
        cache: &mut TypeCache,
    ) -> PoolIndex<Function> {
        // callback methods must use the same parameter names as the base method they override
        let rename_param = match self.base {
            Some(base) if self.flags.is_callback() => pool
                .function(base)
                .unwrap()
                .parameters
                .first()
                .map(|param| pool.def_name(*param).unwrap()),
            _ => None,
        };

        let id = pool.reserve();
        let name = pool.names.add(self.name);

        let return_type = matches!(self.return_type, Type::Prim(Prim::Unit))
            .not()
            .then(|| cache.alloc_type(&self.return_type, repo, pool));

        let parameters = match rename_param {
            Some(rename) => self
                .params
                .into_iter()
                .map(|param| param.renamed(rename.clone()).commit(id, repo, pool, cache))
                .collect(),
            None => self
                .params
                .into_iter()
                .map(|param| param.commit(id, repo, pool, cache))
                .collect(),
        };

        let locals = self
            .locals
            .into_iter()
            .map(|local| local.commit(repo, pool, cache))
            .collect();

        let def = Function {
            visibility: self.visibility,
            flags: self
                .flags
                .with_is_callback(self.flags.is_callback() && !self.is_wrapper),
            source: Some(SourceReference {
                file: PoolIndex::DEFAULT_SOURCE,
                line: 0,
            }),
            return_type,
            unk1: false,
            base_method: self.base,
            parameters,
            locals,
            operator: None,
            cast: 0,
            code: self.body,
            unk2: vec![],
        };

        pool.put_definition(id, Definition::function(name, parent, def));
        id
    }

    #[inline]
    pub fn with_wrapper_flag(self) -> Self {
        Self {
            is_wrapper: true,
            ..self
        }
    }
}

#[derive(Debug, TypedBuilder)]
pub struct FieldBuilder<'id> {
    #[builder(default = FieldFlags::new())]
    flags: FieldFlags,
    #[builder(default = Visibility::Private)]
    visibility: Visibility,
    #[builder(setter(into))]
    name: Str,
    typ: Type<'id>,
}

impl<'id> FieldBuilder<'id> {
    pub fn commit(
        self,
        parent: PoolIndex<Class>,
        repo: &TypeRepo<'id>,
        pool: &mut ConstantPool,
        cache: &mut TypeCache,
    ) -> PoolIndex<Field> {
        let name = pool.names.add(self.name);
        let type_ = cache.alloc_type(&self.typ, repo, pool);
        let def = Field {
            visibility: self.visibility,
            type_,
            flags: self.flags,
            hint: None,
            attributes: vec![],
            defaults: vec![],
        };
        pool.add_definition(Definition::field(name, parent, def))
    }
}

#[derive(Debug, TypedBuilder)]
pub struct ParamBuilder<'id> {
    #[builder(default = ParameterFlags::new())]
    flags: ParameterFlags,
    #[builder(setter(into))]
    name: Str,
    typ: Type<'id>,
}

impl<'id> ParamBuilder<'id> {
    pub fn commit(
        self,
        parent: PoolIndex<Function>,
        repo: &TypeRepo<'id>,
        pool: &mut ConstantPool,
        cache: &mut TypeCache,
    ) -> PoolIndex<Parameter> {
        let name = pool.names.add(self.name);
        let type_ = cache.alloc_type(&self.typ, repo, pool);
        let def = Parameter {
            type_,
            flags: self.flags,
        };
        pool.add_definition(Definition::param(name, parent, def))
    }

    #[inline]
    pub fn renamed(self, name: Str) -> Self {
        Self { name, ..self }
    }
}

#[derive(Debug, TypedBuilder)]
pub struct LocalBuilder<'id> {
    #[builder(default = LocalFlags::new())]
    flags: LocalFlags,
    #[builder(setter(into))]
    name: Str,
    typ: Type<'id>,
}

impl<'id> LocalBuilder<'id> {
    pub fn commit(self, repo: &TypeRepo<'id>, pool: &mut ConstantPool, cache: &mut TypeCache) -> PoolIndex<Local> {
        let name = pool.names.add(self.name);
        let type_ = cache.alloc_type(&self.typ, repo, pool);
        let def = Local {
            type_,
            flags: self.flags,
        };
        pool.add_definition(Definition::local(name, PoolIndex::UNDEFINED, def))
    }
}

#[derive(Debug, Default)]
pub struct TypeCache {
    types: HashMap<Str, PoolIndex<PoolType>>,
}

impl TypeCache {
    pub fn add(&mut self, mangled: Str, idx: PoolIndex<PoolType>) {
        self.types.insert(mangled, idx);
    }

    pub fn alloc_type<'id>(
        &mut self,
        typ: &Type<'id>,
        repo: &TypeRepo<'id>,
        pool: &mut ConstantPool,
    ) -> PoolIndex<PoolType> {
        if matches!(typ, Type::Var(_) | Type::Bottom | Type::Top)
            || matches!(typ, Type::Data(data) if matches!(repo.get_type(data.id), Some(DataType::Class(class)) if !class.flags.is_struct()))
        {
            let data = Type::Data(Parameterized::new(predef::REF, Rc::new([typ.clone()])));
            self.alloc_type_unwrapped(&data, repo, pool)
        } else {
            self.alloc_type_unwrapped(typ, repo, pool)
        }
    }

    fn alloc_type_unwrapped<'id>(
        &mut self,
        typ: &Type<'id>,
        repo: &TypeRepo<'id>,
        pool: &mut ConstantPool,
    ) -> PoolIndex<PoolType> {
        let name = serialize_type(typ, repo, true);
        self.types
            .get(name.as_ref().either(Deref::deref, Str::as_str))
            .copied()
            .unwrap_or_else(|| self.add_type(name.either_into(), typ, repo, pool))
    }

    fn add_type<'id>(
        &mut self,
        name: Str,
        typ: &Type<'id>,
        repo: &TypeRepo<'id>,
        pool: &mut ConstantPool,
    ) -> PoolIndex<PoolType> {
        let pool_type = match typ {
            Type::Data(data) => match (repo.get_type(data.id).unwrap(), &data.args[..]) {
                (DataType::Builtin { .. }, [arg]) => match data.id {
                    id if id == predef::REF => PoolType::Ref(self.alloc_type_unwrapped(arg, repo, pool)),
                    id if id == predef::WREF => PoolType::WeakRef(self.alloc_type_unwrapped(arg, repo, pool)),
                    id if id == predef::ARRAY => PoolType::Array(self.alloc_type(arg, repo, pool)),
                    id if id == predef::SCRIPT_REF => PoolType::ScriptRef(self.alloc_type(arg, repo, pool)),
                    _ => unreachable!(),
                },
                _ => PoolType::Class,
            },
            Type::Bottom | Type::Top | Type::Var(_) => PoolType::Class,
            Type::Prim(_) => PoolType::Prim,
        };
        let name_idx = pool.names.add(name.clone());
        let type_idx = pool.add_definition(Definition::type_(name_idx, pool_type));
        self.types.insert(name, type_idx);
        type_idx
    }
}

fn serialize_type<'id>(typ: &Type<'id>, repo: &TypeRepo<'id>, unwrapped: bool) -> Either<&'id str, Str> {
    match typ {
        Type::Data(typ) => match repo.get_type(typ.id).unwrap() {
            _ if typ.id == predef::REF || typ.id == predef::WREF => {
                Either::Right(str_fmt!("{}:{}", typ.id, serialize_type(&typ.args[0], repo, true)))
            }
            DataType::Builtin { .. } if !typ.args.is_empty() => {
                Either::Right(str_fmt!("{}:{}", typ.id, serialize_type(&typ.args[0], repo, false)))
            }
            DataType::Class(class) if !class.flags.is_struct() && !unwrapped => {
                Either::Right(str_fmt!("ref:{}", typ.id))
            }
            _ => Either::Left(typ.id.as_str()),
        },
        Type::Prim(prim) => Either::Left(prim.into()),
        Type::Bottom | Type::Top | Type::Var(_) if unwrapped => Either::Left("IScriptable"),
        Type::Bottom | Type::Top | Type::Var(_) => Either::Left("ref:IScriptable"),
    }
}
