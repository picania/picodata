use crate::schema::{
    Distribution, PrivilegeDef, RoutineLanguage, RoutineParams, RoutineSecurity, UserDef, ADMIN_ID,
    GUEST_ID, PUBLIC_ID, SUPER_ID,
};
use crate::storage::Clusterwide;
use crate::storage::{space_by_name, RoutineId};
use crate::traft::error::Error as TRaftError;
use ::tarantool::auth::AuthDef;
use ::tarantool::index::{IndexId, Part};
use ::tarantool::space::{Field, SpaceId};
use ::tarantool::tlua;
use ::tarantool::tuple::{ToTupleBuffer, TupleBuffer};
use serde::{Deserialize, Serialize};
use tarantool::session::UserId;
use tarantool::space::SpaceEngineType;

////////////////////////////////////////////////////////////////////////////////
/// The operation on the raft state machine.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "kind")]
pub enum Op {
    /// No operation.
    Nop,
    /// Cluster-wide data modification operation.
    /// Should be used to manipulate the cluster-wide configuration.
    Dml(Dml),
    /// Batch cluster-wide data modification operation.
    /// TODO: use batch not only for dml operations, currently
    /// we can't do ACL together with DML because ACL requires
    /// to be checked on tarantool replicaset leader.
    BatchDml { ops: Vec<Dml> },
    /// Start cluster-wide data schema definition operation.
    /// Should be used to manipulate the cluster-wide schema.
    ///
    /// The provided DDL operation will be set as pending.
    /// Only one pending DDL operation can exist at the same time.
    DdlPrepare { schema_version: u64, ddl: Ddl },
    /// Commit the pending DDL operation.
    ///
    /// Only one pending DDL operation can exist at the same time.
    DdlCommit,
    /// Abort the pending DDL operation.
    ///
    /// Only one pending DDL operation can exist at the same time.
    DdlAbort,
    /// Cluster-wide access control list change operation.
    Acl(Acl),
}

/// Helper struct for serializing subarray of dml
/// commands to avoid copying. It must serialize
/// to the same stuff as `BatchDml`.
#[derive(Serialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "kind")]
pub enum BatchRef<'ops> {
    BatchDml { ops: &'ops [Dml] },
}

#[cfg(test)]
#[test]
fn check_batch_dml_serialize() {
    // check BatchDml and BatchRef are serialized the same way
    let dmls = vec![
        Dml::insert(0u32, &[0, 1], 1u32).unwrap(),
        Dml::insert(0u32, &[0, 1], 0u32).unwrap(),
    ];
    let reffed = BatchRef::BatchDml {
        ops: &dmls[0..dmls.len()],
    };
    let ser_actual = rmp_serde::to_vec_named(&reffed).unwrap();
    let owned = Op::BatchDml { ops: dmls.clone() };
    let ser_expected = rmp_serde::to_vec_named(&owned).unwrap();
    assert_eq!(ser_expected, ser_actual);

    let deser_actual: Op = rmp_serde::from_slice(&ser_actual).unwrap();
    assert!(matches!(deser_actual, Op::BatchDml { .. }));
    if let Op::BatchDml { ops } = deser_actual {
        assert_eq!(ops, dmls);
    }
}

impl std::fmt::Display for Op {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        return match self {
            Self::Nop => f.write_str("Nop"),
            Self::BatchDml { ops } => {
                for op in ops {
                    match op {
                        Dml::Insert { table, tuple, .. } => {
                            writeln!(f, "Insert({table}, {})", DisplayAsJson(tuple))?;
                        }
                        Dml::Replace { table, tuple, .. } => {
                            writeln!(f, "Replace({table}, {})", DisplayAsJson(tuple))?;
                        }
                        Dml::Update {
                            table, key, ops, ..
                        } => {
                            let key = DisplayAsJson(key);
                            let ops = DisplayAsJson(&**ops);
                            writeln!(f, "Update({table}, {key}, {ops})")?;
                        }
                        Dml::Delete { table, key, .. } => {
                            writeln!(f, "Delete({table}, {})", DisplayAsJson(key))?;
                        }
                    }
                }
                Ok(())
            }
            Self::Dml(Dml::Insert { table, tuple, .. }) => {
                write!(f, "Insert({table}, {})", DisplayAsJson(tuple))
            }
            Self::Dml(Dml::Replace { table, tuple, .. }) => {
                write!(f, "Replace({table}, {})", DisplayAsJson(tuple))
            }
            Self::Dml(Dml::Update {
                table, key, ops, ..
            }) => {
                let key = DisplayAsJson(key);
                let ops = DisplayAsJson(&**ops);
                write!(f, "Update({table}, {key}, {ops})")
            }
            Self::Dml(Dml::Delete { table, key, .. }) => {
                write!(f, "Delete({table}, {})", DisplayAsJson(key))
            }
            Self::DdlPrepare {
                schema_version,
                ddl: Ddl::CreateTable {
                    id, distribution, ..
                },
            } => {
                let distr = match distribution {
                    Distribution::Global => "Global",
                    Distribution::ShardedImplicitly { .. } => "ShardedImplicitly",
                    Distribution::ShardedByField { .. } => "ShardedByField",
                };
                write!(
                    f,
                    "DdlPrepare({schema_version}, CreateTable({id}, {distr}))"
                )
            }
            Self::DdlPrepare {
                schema_version,
                ddl: Ddl::DropTable { id, .. },
            } => {
                write!(f, "DdlPrepare({schema_version}, DropTable({id}))")
            }
            Self::DdlPrepare {
                schema_version,
                ddl: Ddl::CreateIndex {
                    space_id, index_id, ..
                },
            } => {
                write!(
                    f,
                    "DdlPrepare({schema_version}, CreateIndex({space_id}, {index_id}))"
                )
            }
            Self::DdlPrepare {
                schema_version,
                ddl: Ddl::DropIndex { space_id, index_id },
            } => {
                write!(
                    f,
                    "DdlPrepare({schema_version}, DropIndex({space_id}, {index_id}))"
                )
            }
            Self::DdlPrepare {
                schema_version,
                ddl: Ddl::CreateProcedure { id, name, .. },
            } => {
                write!(
                    f,
                    "DdlPrepare({schema_version}, CreateProcedure({id}, {name}))"
                )
            }
            Self::DdlPrepare {
                schema_version,
                ddl: Ddl::DropProcedure { id, .. },
            } => {
                write!(f, "DdlPrepare({schema_version}, DropProcedure({id}))")
            }
            Self::DdlPrepare {
                schema_version,
                ddl:
                    Ddl::RenameProcedure {
                        routine_id,
                        old_name,
                        new_name,
                        ..
                    },
            } => {
                write!(
                    f,
                    "DdlPrepare({schema_version}, CreateProcedure({routine_id}, {old_name} -> {new_name}))"
                )
            }
            Self::DdlCommit => write!(f, "DdlCommit"),
            Self::DdlAbort => write!(f, "DdlAbort"),
            Self::Acl(Acl::CreateUser { user_def }) => {
                let UserDef {
                    id,
                    name,
                    schema_version,
                    ..
                } = user_def;
                write!(f, r#"CreateUser({schema_version}, {id}, "{name}")"#,)
            }
            Self::Acl(Acl::RenameUser {
                user_id,
                name,
                schema_version,
                ..
            }) => {
                write!(f, r#"RenameUser({schema_version}, {user_id}, "{name}")"#,)
            }
            Self::Acl(Acl::ChangeAuth {
                user_id,
                initiator,
                schema_version,
                ..
            }) => {
                write!(f, "ChangeAuth({schema_version}, {user_id}, {initiator})")
            }
            Self::Acl(Acl::DropUser {
                user_id,
                initiator,
                schema_version,
            }) => {
                write!(f, "DropUser({schema_version}, {user_id} {initiator})")
            }
            Self::Acl(Acl::CreateRole { role_def }) => {
                let UserDef {
                    id,
                    name,
                    schema_version,
                    ..
                } = role_def;
                write!(f, r#"CreateRole({schema_version}, {id}, "{name}")"#,)
            }
            Self::Acl(Acl::DropRole {
                role_id,
                schema_version,
                ..
            }) => {
                write!(f, "DropRole({schema_version}, {role_id})")
            }
            Self::Acl(Acl::GrantPrivilege { priv_def }) => {
                let object_id = priv_def.object_id();

                write!(
                    f,
                    "GrantPrivilege({schema_version}, {grantor_id}, {grantee_id}, {object_type}, {object_id:?}, {privilege})",
                    schema_version = priv_def.schema_version(),
                    grantor_id = priv_def.grantor_id(),
                    grantee_id = priv_def.grantee_id(),
                    object_type = priv_def.object_type(),
                    privilege = priv_def.privilege(),
                )
            }
            Self::Acl(Acl::RevokePrivilege { priv_def, .. }) => {
                let object_id = priv_def.object_id();
                write!(
                    f,
                    "RevokePrivilege({schema_version}, {grantor_id}, {grantee_id}, {object_type}, {object_id:?}, {privilege})",
                    schema_version = priv_def.schema_version(),
                    grantor_id = priv_def.grantor_id(),
                    grantee_id = priv_def.grantee_id(),
                    object_type = priv_def.object_type(),
                    privilege = priv_def.privilege(),)
            }
        };

        struct DisplayAsJson<T>(pub T);

        impl std::fmt::Display for DisplayAsJson<&TupleBuffer> {
            fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                if let Some(data) = rmp_serde::from_slice::<serde_json::Value>(self.0.as_ref())
                    .ok()
                    .and_then(|v| serde_json::to_string(&ValueWithTruncations(&v)).ok())
                {
                    return write!(f, "{data}");
                }

                write!(f, "{:?}", self.0)
            }
        }

        const TRUNCATION_THRESHOLD_FOR_STRING: usize = 100;
        const TRUNCATION_THRESHOLD_FOR_ARRAY: usize = 10;
        const TRUNCATION_THRESHOLD_FOR_MAP: usize = 10;
        struct ValueWithTruncations<'a>(&'a serde_json::Value);
        impl Serialize for ValueWithTruncations<'_> {
            #[inline]
            fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
            where
                S: ::serde::Serializer,
            {
                use serde_json::Value;

                match self.0 {
                    Value::Null => serializer.serialize_unit(),
                    Value::Bool(b) => serializer.serialize_bool(*b),
                    Value::Number(n) => n.serialize(serializer),
                    Value::String(s) => {
                        let threshold = TRUNCATION_THRESHOLD_FOR_STRING;
                        if s.len() > threshold {
                            let s = format!("{}<TRUNCATED>...", &s[..threshold]);
                            serializer.serialize_str(&s)
                        } else {
                            serializer.serialize_str(s)
                        }
                    }
                    Value::Array(v) => {
                        let threshold = TRUNCATION_THRESHOLD_FOR_ARRAY;
                        if v.len() > threshold {
                            let mut t = Vec::with_capacity(threshold + 1);
                            t.extend_from_slice(&v[..threshold]);
                            t.push(Value::from("<TRUNCATED>"));
                            t.serialize(serializer)
                        } else {
                            v.serialize(serializer)
                        }
                    }
                    Value::Object(m) => {
                        use serde::ser::SerializeMap;
                        let mut map = serializer.serialize_map(Some(m.len()))?;
                        let threshold = TRUNCATION_THRESHOLD_FOR_MAP;
                        for (k, v) in m.iter().take(threshold) {
                            map.serialize_entry(k, v)?;
                        }
                        if m.len() > threshold {
                            map.serialize_entry(
                                &Value::from("<TRUNCATED>"),
                                &Value::from("<TRUNCATED>"),
                            )?;
                        }
                        map.end()
                    }
                }
            }
        }

        impl std::fmt::Display for DisplayAsJson<&[TupleBuffer]> {
            fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "[")?;
                if let Some(elem) = self.0.first() {
                    write!(f, "{}", DisplayAsJson(elem))?;
                }
                for elem in self.0.iter().skip(1) {
                    write!(f, ", {}", DisplayAsJson(elem))?;
                }
                write!(f, "]")
            }
        }
    }
}

impl Op {
    #[inline]
    pub fn is_schema_change(&self) -> bool {
        match self {
            Self::Nop | Self::Dml(_) | Self::DdlAbort | Self::DdlCommit | Self::BatchDml { .. } => {
                false
            }
            Self::DdlPrepare { .. } | Self::Acl(_) => true,
        }
    }
}

////////////////////////////////////////////////////////////////////////////////
// Dml
////////////////////////////////////////////////////////////////////////////////

/// Cluster-wide data modification operation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "op_kind")]
pub enum Dml {
    Insert {
        table: SpaceId,
        #[serde(with = "serde_bytes")]
        tuple: TupleBuffer,
        initiator: UserId,
    },
    Replace {
        table: SpaceId,
        #[serde(with = "serde_bytes")]
        tuple: TupleBuffer,
        initiator: UserId,
    },
    Update {
        table: SpaceId,
        /// Key in primary index
        #[serde(with = "serde_bytes")]
        key: TupleBuffer,
        #[serde(with = "vec_of_raw_byte_buf")]
        ops: Vec<TupleBuffer>,
        initiator: UserId,
    },
    Delete {
        table: SpaceId,
        /// Key in primary index
        #[serde(with = "serde_bytes")]
        key: TupleBuffer,
        initiator: UserId,
    },
}

impl Dml {
    pub fn initiator(&self) -> UserId {
        match self {
            Dml::Insert { initiator, .. } => *initiator,
            Dml::Replace { initiator, .. } => *initiator,
            Dml::Update { initiator, .. } => *initiator,
            Dml::Delete { initiator, .. } => *initiator,
        }
    }
}

::tarantool::define_str_enum! {
    pub enum DmlKind {
        Insert = "insert",
        Replace = "replace",
        Update = "update",
        Delete = "delete",
    }
}

impl From<Dml> for Op {
    fn from(op: Dml) -> Op {
        Op::Dml(op)
    }
}

impl Dml {
    /// Serializes `tuple` and returns an [`Dml::Insert`] in case of success.
    #[inline(always)]
    pub fn insert(
        space: impl Into<SpaceId>,
        tuple: &impl ToTupleBuffer,
        initiator: UserId,
    ) -> tarantool::Result<Self> {
        let res = Self::Insert {
            table: space.into(),
            tuple: tuple.to_tuple_buffer()?,
            initiator,
        };
        Ok(res)
    }

    /// Serializes `tuple` and returns an [`Dml::Replace`] in case of success.
    #[inline(always)]
    pub fn replace(
        space: impl Into<SpaceId>,
        tuple: &impl ToTupleBuffer,
        initiator: UserId,
    ) -> tarantool::Result<Self> {
        let res = Self::Replace {
            table: space.into(),
            tuple: tuple.to_tuple_buffer()?,
            initiator,
        };
        Ok(res)
    }

    /// Serializes `key` and returns an [`Dml::Update`] in case of success.
    #[inline(always)]
    pub fn update(
        space: impl Into<SpaceId>,
        key: &impl ToTupleBuffer,
        ops: impl Into<Vec<TupleBuffer>>,
        initiator: UserId,
    ) -> tarantool::Result<Self> {
        let res = Self::Update {
            table: space.into(),
            key: key.to_tuple_buffer()?,
            ops: ops.into(),
            initiator,
        };
        Ok(res)
    }

    /// Serializes `key` and returns an [`Dml::Delete`] in case of success.
    #[inline(always)]
    pub fn delete(
        space: impl Into<SpaceId>,
        key: &impl ToTupleBuffer,
        initiator: UserId,
    ) -> tarantool::Result<Self> {
        let res = Self::Delete {
            table: space.into(),
            key: key.to_tuple_buffer()?,
            initiator,
        };
        Ok(res)
    }

    #[rustfmt::skip]
    pub fn space(&self) -> SpaceId {
        match self {
            Self::Insert { table, .. } => *table,
            Self::Replace { table, .. } => *table,
            Self::Update { table, .. } => *table,
            Self::Delete { table, .. } => *table,
        }
    }

    /// Parse lua arguments to an api function such as `pico.cas`.
    pub fn from_lua_args(op: DmlInLua, initiator: UserId) -> Result<Self, String> {
        let space = space_by_name(&op.table).map_err(|e| e.to_string())?;
        let table = space.id();
        match op.kind {
            DmlKind::Insert => {
                let Some(tuple) = op.tuple else {
                    return Err("insert operation must have a tuple".into());
                };
                Ok(Self::Insert {
                    table,
                    tuple,
                    initiator,
                })
            }
            DmlKind::Replace => {
                let Some(tuple) = op.tuple else {
                    return Err("replace operation must have a tuple".into());
                };
                Ok(Self::Replace {
                    table,
                    tuple,
                    initiator,
                })
            }
            DmlKind::Update => {
                let Some(key) = op.key else {
                    return Err("update operation must have a key".into());
                };
                let Some(ops) = op.ops else {
                    return Err("update operation must have ops".into());
                };
                Ok(Self::Update {
                    table,
                    key,
                    ops,
                    initiator,
                })
            }
            DmlKind::Delete => {
                let Some(key) = op.key else {
                    return Err("delete operation must have a key".into());
                };
                Ok(Self::Delete {
                    table,
                    key,
                    initiator,
                })
            }
        }
    }
}

/// Represents a lua table describing a [`Dml`] operation.
///
/// This is only used to parse lua arguments from lua api functions such as
/// `pico.cas`.
#[derive(Clone, Debug, PartialEq, Eq, tlua::LuaRead)]
pub struct DmlInLua {
    pub table: String,
    pub kind: DmlKind,
    pub tuple: Option<TupleBuffer>,
    pub key: Option<TupleBuffer>,
    pub ops: Option<Vec<TupleBuffer>>,
}

#[derive(Clone, Debug, PartialEq, Eq, tlua::LuaRead)]
pub struct BatchDmlInLua {
    pub ops: Vec<DmlInLua>,
}

////////////////////////////////////////////////////////////////////////////////
// Ddl
////////////////////////////////////////////////////////////////////////////////

/// Represents Ddl operations performed on the cluster.
///
/// Note: for the purpose of audit log in some variants we keep initiator field.
/// For Create<...> operations initiator and owner are the same,
/// so owner is used to avoid duplication.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "kind")]
pub enum Ddl {
    CreateTable {
        id: SpaceId,
        name: String,
        format: Vec<Field>,
        primary_key: Vec<Part>,
        distribution: Distribution,
        engine: SpaceEngineType,
        owner: UserId,
    },
    DropTable {
        id: SpaceId,
        initiator: UserId,
    },
    CreateIndex {
        space_id: SpaceId,
        index_id: IndexId,
        by_fields: Vec<Part>,
    },
    DropIndex {
        space_id: SpaceId,
        index_id: IndexId,
    },
    CreateProcedure {
        id: RoutineId,
        name: String,
        params: RoutineParams,
        language: RoutineLanguage,
        body: String,
        security: RoutineSecurity,
        owner: UserId,
    },
    DropProcedure {
        id: RoutineId,
        initiator: UserId,
    },
    RenameProcedure {
        routine_id: u32,
        old_name: String,
        new_name: String,
        initiator_id: UserId,
        owner_id: UserId,
        schema_version: u64,
    },
}

/// Builder for [`Op::DdlPrepare`] operations.
///
/// # Example
/// ```no_run
/// use picodata::traft::op::{DdlBuilder, Ddl};
///
/// // Assuming that space `1` was created.
/// let op = DdlBuilder::with_schema_version(1)
///     .with_op(Ddl::DropTable { id: 1, initiator: 1 });
/// ```
pub struct DdlBuilder {
    schema_version: u64,
}

impl DdlBuilder {
    pub fn new(storage: &Clusterwide) -> super::Result<Self> {
        let version = storage.properties.next_schema_version()?;
        Ok(Self::with_schema_version(version))
    }

    /// Sets current schema version.
    pub fn with_schema_version(version: u64) -> Self {
        Self {
            schema_version: version,
        }
    }

    pub fn with_op(&self, op: Ddl) -> Op {
        Op::DdlPrepare {
            schema_version: self.schema_version,
            ddl: op,
        }
    }
}

////////////////////////////////////////////////////////////////////////////////
// Acl
////////////////////////////////////////////////////////////////////////////////

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "op_kind")]
pub enum Acl {
    /// Create a tarantool user. Grant it default privileges.
    CreateUser { user_def: UserDef },

    /// Rename a tarantool user.
    RenameUser {
        user_id: UserId,
        name: String,
        initiator: UserId,
        schema_version: u64,
    },

    /// Update the tarantool user's authentication details (e.g. password).
    ChangeAuth {
        user_id: UserId,
        auth: AuthDef,
        initiator: UserId,
        schema_version: u64,
    },

    /// Drop a tarantool user and any entities owned by it.
    DropUser {
        user_id: UserId,
        initiator: UserId,
        schema_version: u64,
    },

    /// Create a tarantool role. Grant it default privileges.
    CreateRole { role_def: UserDef },

    /// Drop a tarantool role and revoke it from any grantees.
    DropRole {
        role_id: UserId,
        initiator: UserId,
        schema_version: u64,
    },

    /// Grant some privilege to a user or a role.
    GrantPrivilege { priv_def: PrivilegeDef },

    /// Revoke some privilege from a user or a role.
    RevokePrivilege {
        priv_def: PrivilegeDef,
        initiator: UserId,
    },
}

impl Acl {
    pub fn schema_version(&self) -> u64 {
        match self {
            Self::CreateUser { user_def } => user_def.schema_version,
            Self::RenameUser { schema_version, .. } => *schema_version,
            Self::ChangeAuth { schema_version, .. } => *schema_version,
            Self::DropUser { schema_version, .. } => *schema_version,
            Self::CreateRole { role_def, .. } => role_def.schema_version,
            Self::DropRole { schema_version, .. } => *schema_version,
            Self::GrantPrivilege { priv_def } => priv_def.schema_version(),
            Self::RevokePrivilege { priv_def, .. } => priv_def.schema_version(),
        }
    }

    /// Performs preliminary checks on acl so that it will not fail when applied.
    /// These checks do not include authorization checks, which are done separately in
    /// [`crate::access_control::access_check_op`].
    pub fn validate(&self) -> Result<(), TRaftError> {
        // THOUGHT: should we move access_check_* fns here as it's done in tarantool?
        match self {
            Self::ChangeAuth { user_id, .. } => {
                // See https://git.picodata.io/picodata/tarantool/-/blob/da5ad0fa3ab8940f524cfa9bf3d582347c01fc4a/src/box/alter.cc#L2925
                if *user_id == GUEST_ID {
                    return Err(TRaftError::other(
                        "altering guest user's password is not allowed",
                    ));
                }
            }
            Self::DropUser { user_id, .. } => {
                // See https://git.picodata.io/picodata/tarantool/-/blob/da5ad0fa3ab8940f524cfa9bf3d582347c01fc4a/src/box/alter.cc#L3080
                if *user_id == GUEST_ID || *user_id == ADMIN_ID {
                    return Err(TRaftError::other("dropping system user is not allowed"));
                }
                // user_has_data will be successful in any case https://git.picodata.io/picodata/tarantool/-/blob/da5ad0fa3ab8940f524cfa9bf3d582347c01fc4a/src/box/alter.cc#L2846
                // as box.schema.user.drop(..) deletes all the related spaces/priveleges/etc.
            }

            Self::DropRole { role_id, .. } => {
                // See https://git.picodata.io/picodata/tarantool/-/blob/da5ad0fa3ab8940f524cfa9bf3d582347c01fc4a/src/box/alter.cc#L3080
                if *role_id == PUBLIC_ID || *role_id == SUPER_ID {
                    return Err(TRaftError::other("dropping system role is not allowed"));
                }
            }
            _ => (),
        }
        Ok(())
    }
}

mod vec_of_raw_byte_buf {
    use super::TupleBuffer;
    use serde::de::Error as _;
    use serde::ser::SerializeSeq;
    use serde::{self, Deserialize, Deserializer, Serializer};
    use serde_bytes::{ByteBuf, Bytes};
    use std::convert::TryFrom;

    pub fn serialize<S>(v: &[TupleBuffer], ser: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = ser.serialize_seq(Some(v.len()))?;
        for buf in v {
            seq.serialize_element(Bytes::new(buf.as_ref()))?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D>(de: D) -> Result<Vec<TupleBuffer>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let tmp = Vec::<ByteBuf>::deserialize(de)?;
        // FIXME(gmoshkin): redundant copy happens here,
        // because ByteBuf and TupleBuffer are essentially the same struct,
        // but there's no easy foolproof way
        // to convert a Vec<ByteBuf> to Vec<TupleBuffer>
        // because of borrow and drop checkers
        let res: tarantool::Result<_> = tmp
            .into_iter()
            .map(|bb| TupleBuffer::try_from(bb.into_vec()))
            .collect();
        res.map_err(D::Error::custom)
    }
}
