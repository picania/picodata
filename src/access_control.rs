//! Access control
//!
//! This module implements access control checks according to picodata access control model.
//!
//! Currently objects we control access to include tables, users and roles. The model closely follows one from
//! vanilla tarantool with some deviations. We build on top of tarantool infrastructure, i e in vanilla tarantool
//! permissions are represented with bit masks for each pair of object and user (for more context see `user_def.h`).
//! So we do not reimplement it in picodata, instead we have two functions exported from vanilla tarantool named
//! `box_access_check_space` and `box_access_check_ddl`. The first one as the name suggests implements checks for
//! DML operations on spaces, the second one allows to check remaining permissions with exception of some edge cases
//! that are handled separately in `access_check_grant_revoke` below. `access_check_grant_revoke` closely follows
//! `priv_def_check` from `alter.cc`. In vanilla ddl permission checks are performed via triggers attached to system spaces.
//! for example `on_replace_dd_space` or `on_replace_dd_priv`. Checks implemented in this module are 1:1 mapping of those.
//!
//! Since picodata is a distributed database we perform clusterwide operations in a coordinated fashion using compare and swap
//! building block. So every clusterwide operation has to go through raft leader that takes care of safely distributing this
//! operation across all instances. Because of that we have a distinction, privileges that are related to operations that change
//! state of the cluster (e g via creating users, new tables, writing to global tables) are checked before going forward with CaS
//! operation. On the other size permissions that do not change state of the cluster (reading and writing to sharded tables)
//! are handled by our sql processing logic in `sql.rs`.
//!
//! Executing CaS operation involves coordination between nodes of the cluster, since cluster nodes authenticate with each other
//! without involving end user credentials we need to pass the user who initiated the request along with the request to raft
//! leader so it can perform the access check. In order for this to work properly node that initiates the request needs to access
//! certain system spaces (see [`crate::cas::compare_and_swap`] for details). Connected users do not have such a permission from vanilla
//! tarantool perspective so we bypass vanilla checks by using `box.session.su(ADMIN)`. Then on the raft leader box.session.su
//! is used again to switch to user provided in the request. This is needed because tarantool functions we use for access checks
//! make them based on effective user.
use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
};

use tarantool::{
    access_control::{
        box_access_check_ddl, box_access_check_space, PrivType,
        SchemaObjectType as TntSchemaObjectType,
    },
    session::{self, UserId},
    space::{Space, SystemSpace},
    tuple::Encode,
};

use crate::{
    schema::{PrivilegeDef, PrivilegeType, SchemaObjectType as PicoSchemaObjectType},
    storage::{space_by_id, Clusterwide, ToEntryIter},
    traft::op::{self, Op},
    ADMIN_USER_ID,
};

tarantool::define_str_enum! {
    pub enum UserMetadataKind {
        User = "user",
        Role = "role",
    }
}

/// User metadata. Represents a tuple of a system `_user` space.
/// TODO move to tarantool-module along with some space specific methods from storage.rs
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct UserMetadata {
    pub id: UserId,
    pub owner_id: UserId,
    pub name: String,
    #[serde(rename = "type")]
    pub ty: UserMetadataKind,
    pub auth: HashMap<String, String>,
    // Note: no way to see the real type of this thing.
    // It is an undocumented enterprise edition only field.
    // For msgpack encoding to match needs to be a sequence
    pub auth_history: [(); 0],
    pub last_modified: usize,
}

impl Encode for UserMetadata {}

fn make_no_such_user(name: &str) -> tarantool::error::Error {
    tarantool::set_error!(
        tarantool::error::TarantoolErrorCode::NoSuchUser,
        "no user named {}",
        name
    );
    tarantool::error::TarantoolError::last().into()
}

/// The function produces an error that has the same format as one that is generated
/// by vanilla tarantool. See original definition in errcode.h
fn make_access_denied(
    access_name: impl Display,
    object_type: PicoSchemaObjectType,
    object_name: impl Display,
    user_name: impl Display,
) -> tarantool::error::Error {
    tarantool::set_error!(
        tarantool::error::TarantoolErrorCode::AccessDenied,
        "{} to {} '{}' is denied for user '{}'",
        access_name,
        object_type,
        object_name,
        user_name
    );
    return tarantool::error::TarantoolError::last().into();
}

pub fn user_by_id(id: UserId) -> tarantool::Result<UserMetadata> {
    let sys_user = Space::from(SystemSpace::User).get(&(id,))?;

    match sys_user {
        Some(u) => u.decode(),
        None => {
            tarantool::set_error!(
                tarantool::error::TarantoolErrorCode::NoSuchUser,
                "no such user #{}",
                id
            );
            return Err(tarantool::error::TarantoolError::last().into());
        }
    }
}

/// There are no cases when box_access_check_ddl is called several times
/// in a row so it is ok that we need to switch once to user who initiated the request
/// This wrapper is needed because usually before checking permissions we need to
/// retrieve some metadata about the object being tested for access and this requires
/// access to system spaces which original user is not required to have.
///
/// # Panicking
///
/// Note that not all combinations of parameters are valid.
/// For in depth description of cases when this function may panic see
/// [box_access_check_ddl](::tarantool::access_control::box_access_check_ddl) in tarantool module.
fn box_access_check_ddl_as_user(
    object_name: &str,
    object_id: u32,
    owner_id: u32,
    object_type: TntSchemaObjectType,
    access: PrivType,
    as_user: UserId,
) -> tarantool::Result<()> {
    let _su = session::su(as_user);

    box_access_check_ddl(object_name, object_id, owner_id, object_type, access)
}

fn access_check_dml(dml: &op::Dml, as_user: UserId) -> tarantool::Result<()> {
    let _su = session::su(as_user).expect("shouldnt fail");
    box_access_check_space(dml.space(), PrivType::Write)
}

/// This function performs access control checks that are identical to ones performed in
/// vanilla tarantool in on_replace_dd_space and on_replace_dd_index respectively
fn access_check_ddl(ddl: &op::Ddl, as_user: UserId) -> tarantool::Result<()> {
    match ddl {
        op::Ddl::CreateTable {
            id, name, owner, ..
        } => {
            assert_eq!(
                *owner, as_user,
                "when creating objects creator is the owner"
            );

            box_access_check_ddl_as_user(
                name,
                *id,
                *owner,
                TntSchemaObjectType::Space,
                PrivType::Create,
                as_user,
            )
        }
        op::Ddl::DropTable { id, .. } => {
            let space = space_by_id(*id)?;
            let meta = space.meta()?;

            box_access_check_ddl_as_user(
                &space.meta()?.name,
                *id,
                meta.user_id,
                TntSchemaObjectType::Space,
                PrivType::Drop,
                as_user,
            )
        }
        op::Ddl::CreateIndex { space_id, .. } => {
            let space = space_by_id(*space_id)?;
            let meta = space.meta()?;

            box_access_check_ddl_as_user(
                &meta.name,
                *space_id,
                meta.user_id,
                TntSchemaObjectType::Space,
                PrivType::Create,
                as_user,
            )
        }
        op::Ddl::DropIndex { space_id, .. } => {
            let space = space_by_id(*space_id)?;
            let meta = space.meta()?;

            box_access_check_ddl_as_user(
                &meta.name,
                *space_id,
                meta.user_id,
                TntSchemaObjectType::Space,
                PrivType::Drop,
                as_user,
            )
        }
    }
}

/// This is port of vanilla tarantool function box/user.cc::role_check.
fn detect_role_grant_cycles(
    granted_role: &UserMetadata,
    priv_def: &PrivilegeDef,
    storage: &Clusterwide,
) -> tarantool::Result<()> {
    let grantee_id = priv_def.grantee_id();
    let grantee_name = {
        let grantee = user_by_id(grantee_id)?;
        grantee.name
    };

    let role_id_to_grantees_id = {
        let mut hm = HashMap::new();
        for privilege in storage.privileges.iter()? {
            if privilege.object_type() == PicoSchemaObjectType::Role
                && privilege.privilege() == PrivilegeType::Execute
            {
                match privilege.object_id() {
                    Some(object_id) => {
                        hm.entry(object_id as i64)
                            .or_insert(Vec::new())
                            .push(privilege.grantee_id() as i64);
                    }
                    None => continue,
                }
            }
        }

        hm
    };

    let mut visited = HashSet::new();
    visited.insert(grantee_id as i64);

    let mut current_layer = visited.clone();
    // It's BFS with start from grantee_id
    while !current_layer.is_empty() {
        let mut next_layer = HashSet::new();

        for role_id in current_layer.iter() {
            let parents = role_id_to_grantees_id.get(role_id);
            if let Some(parents) = parents {
                next_layer.extend(parents.iter());
            }
        }

        visited.extend(next_layer.iter());
        current_layer = next_layer;
    }

    if visited.contains(&(granted_role.id as i64)) {
        let err = tarantool::set_and_get_error!(
            tarantool::error::TarantoolErrorCode::RoleLoop,
            "Granting role {} to role {} would create a loop",
            granted_role.name,
            grantee_name
        );
        return Err(err.into());
    }

    Ok(())
}

fn access_check_grant_revoke(
    storage: &Clusterwide,
    priv_def: &PrivilegeDef,
    grantor_id: UserId,
    access: PrivType,
    access_name: &str,
) -> tarantool::Result<()> {
    // Note: this is vanilla tarantool user, not picodata one.
    let grantor = user_by_id(grantor_id)?;

    let object_id = match priv_def.object_id() {
        None => {
            // This is a wildcard grant on entire entity type i e grant on all spaces.
            // In vanilla tarantool each object type has counterpart with entity suffix
            // i e BOX_SC_ENTITY_SPACE. Since these variants are not stored we do
            // not materialize them as valid SchemaObjectType variants and streamline this check.
            if grantor_id != ADMIN_USER_ID {
                return Err(make_access_denied(
                    access_name,
                    priv_def.object_type(),
                    "",
                    grantor.name,
                ));
            }
            return Ok(());
        }
        Some(object_id) => object_id,
    };

    match priv_def.object_type() {
        PicoSchemaObjectType::Universe => {
            // Only admin can grant on universe
            if grantor_id != ADMIN_USER_ID {
                return Err(make_access_denied(
                    access_name,
                    PicoSchemaObjectType::Universe,
                    "",
                    grantor.name,
                ));
            }
        }
        PicoSchemaObjectType::Table => {
            let space = space_by_id(object_id)?;
            let meta = space.meta()?;

            assert_eq!(object_id, meta.id, "user metadata id mismatch");

            // Only owner or admin can grant on space
            if meta.user_id != grantor_id && grantor_id != ADMIN_USER_ID {
                return Err(make_access_denied(
                    access_name,
                    PicoSchemaObjectType::Table,
                    meta.name,
                    grantor.name,
                ));
            }

            return box_access_check_ddl_as_user(
                &meta.name,
                meta.id,
                grantor_id,
                TntSchemaObjectType::Space,
                access,
                grantor_id,
            );
        }
        PicoSchemaObjectType::Role => {
            let granted_role = user_by_id(object_id)?;

            assert_eq!(object_id, granted_role.id, "user metadata id mismatch");

            // Only the creator of the role or admin can grant or revoke it.
            // Everyone can grant 'PUBLIC' role.
            // Note that having a role means having execute privilege on it.
            if granted_role.owner_id != grantor_id
                && grantor_id != ADMIN_USER_ID
                && !(granted_role.name == "public"
                    && priv_def.privilege() == PrivilegeType::Execute)
            {
                return Err(make_access_denied(
                    access_name,
                    PicoSchemaObjectType::Role,
                    granted_role.name,
                    grantor.name,
                ));
            }

            detect_role_grant_cycles(&granted_role, priv_def, storage)?;

            return box_access_check_ddl_as_user(
                &granted_role.name,
                granted_role.id,
                grantor_id,
                TntSchemaObjectType::Role,
                access,
                grantor_id,
            );
        }
        PicoSchemaObjectType::User => {
            let target_sys_user = user_by_id(object_id)?;
            if target_sys_user.ty != UserMetadataKind::User {
                return Err(make_no_such_user(&target_sys_user.name));
            }

            // Only owner or admin can grant on user
            if target_sys_user.owner_id != grantor_id && grantor_id != ADMIN_USER_ID {
                return Err(make_access_denied(
                    access_name,
                    PicoSchemaObjectType::User,
                    target_sys_user.name,
                    grantor.name,
                ));
            }

            return box_access_check_ddl_as_user(
                &target_sys_user.name,
                target_sys_user.id,
                grantor_id,
                TntSchemaObjectType::User,
                access,
                grantor_id,
            );
        }
    }

    Ok(())
}

fn access_check_acl(
    storage: &Clusterwide,
    acl: &op::Acl,
    as_user: UserId,
) -> tarantool::Result<()> {
    match acl {
        op::Acl::CreateUser { user_def } => {
            assert_eq!(
                user_def.owner, as_user,
                "when creating objects creator is the owner"
            );
            box_access_check_ddl_as_user(
                &user_def.name,
                user_def.id,
                user_def.owner,
                TntSchemaObjectType::User,
                PrivType::Create,
                as_user,
            )
        }
        op::Acl::ChangeAuth { user_id, .. } => {
            let sys_user = user_by_id(*user_id)?;

            assert_eq!(sys_user.id, *user_id, "user metadata id mismatch");

            box_access_check_ddl_as_user(
                &sys_user.name,
                sys_user.id,
                sys_user.owner_id,
                TntSchemaObjectType::User,
                PrivType::Alter,
                as_user,
            )
        }
        op::Acl::DropUser { user_id, .. } => {
            let sys_user = user_by_id(*user_id)?;

            assert_eq!(sys_user.id, *user_id, "user metadata id mismatch");

            box_access_check_ddl_as_user(
                &sys_user.name,
                sys_user.id,
                sys_user.owner_id,
                TntSchemaObjectType::User,
                PrivType::Drop,
                as_user,
            )
        }
        op::Acl::CreateRole { role_def } => {
            assert_eq!(
                role_def.owner, as_user,
                "when creating objects creator is the owner"
            );
            box_access_check_ddl_as_user(
                &role_def.name,
                role_def.id,
                role_def.owner,
                TntSchemaObjectType::Role,
                PrivType::Create,
                as_user,
            )
        }
        op::Acl::DropRole { role_id, .. } => {
            // In vanilla roles and users are stored in the same space
            // so we can reuse the definition
            let sys_user = user_by_id(*role_id)?;

            assert_eq!(sys_user.id, *role_id, "user metadata id mismatch");

            box_access_check_ddl_as_user(
                &sys_user.name,
                sys_user.id,
                sys_user.owner_id,
                TntSchemaObjectType::Role,
                PrivType::Drop,
                as_user,
            )
        }
        op::Acl::GrantPrivilege { priv_def } => {
            access_check_grant_revoke(storage, priv_def, as_user, PrivType::Grant, "Grant")
        }
        op::Acl::RevokePrivilege { priv_def, .. } => {
            access_check_grant_revoke(storage, priv_def, as_user, PrivType::Revoke, "Revoke")
        }
    }
}

pub(super) fn access_check_op(
    storage: &Clusterwide,
    op: &op::Op,
    as_user: UserId,
) -> tarantool::Result<()> {
    match op {
        Op::Nop => Ok(()),
        Op::Dml(dml) => access_check_dml(dml, as_user),
        Op::DdlPrepare { ddl, .. } => access_check_ddl(ddl, as_user),
        Op::DdlCommit | Op::DdlAbort => {
            if as_user != ADMIN_USER_ID {
                let sys_user = user_by_id(as_user)?;
                return Err(make_access_denied(
                    "ddl",
                    PicoSchemaObjectType::Universe,
                    "",
                    sys_user.name,
                ));
            }
            Ok(())
        }
        Op::Acl(acl) => access_check_acl(storage, acl, as_user),
    }
}

mod tests {
    use std::collections::HashMap;

    use super::{access_check_acl, access_check_ddl, user_by_id};
    use crate::{
        access_control::{access_check_op, UserMetadataKind},
        schema::{
            Distribution, PrivilegeDef, PrivilegeType, RoleDef, SchemaObjectType, UserDef, ADMIN_ID,
        },
        storage::{
            acl::{
                global_create_role, global_grant_privilege, on_master_create_role,
                on_master_create_user, on_master_grant_privilege, on_master_revoke_privilege,
            },
            Clusterwide,
        },
        traft::op::{Acl, Ddl, Dml, Op},
        ADMIN_USER_ID,
    };
    use tarantool::{
        auth::{AuthData, AuthDef, AuthMethod},
        session::{self, UserId},
        space::{Space, SpaceCreateOptions, SpaceEngineType},
        tuple::{Tuple, TupleBuffer},
    };

    static mut NEXT_USER_ID: u32 = 42;

    fn next_user_id() -> u32 {
        // SAFETY: tests are always run on tx thread sequentially
        unsafe {
            NEXT_USER_ID += 1;
            NEXT_USER_ID
        }
    }

    #[tarantool::test]
    fn decode_user_metadata() {
        let sys_user = user_by_id(ADMIN_USER_ID).unwrap();
        assert_eq!(sys_user.id, ADMIN_USER_ID);
        assert_eq!(sys_user.owner_id, ADMIN_USER_ID);
        assert_eq!(&sys_user.name, "admin");
        assert_eq!(sys_user.ty, UserMetadataKind::User);
        assert_eq!(sys_user.auth, HashMap::new());
        assert_eq!(sys_user.auth_history, []);
        assert_eq!(sys_user.last_modified, 0);
    }

    fn dummy_auth_def() -> AuthDef {
        AuthDef::new(
            AuthMethod::ChapSha1,
            AuthData::new(&AuthMethod::ChapSha1, "", "").into_string(),
        )
    }

    fn dummy_user_def(id: UserId, name: String, owner: Option<UserId>) -> UserDef {
        UserDef {
            id,
            name,
            schema_version: 0,
            auth: dummy_auth_def(),
            owner: owner.unwrap_or_else(|| session::uid().unwrap()),
        }
    }

    #[track_caller]
    fn make_user(name: &str, owner: Option<UserId>) -> u32 {
        let id = next_user_id();
        let user_def = dummy_user_def(id, name.to_owned(), owner);
        on_master_create_user(&user_def).unwrap();
        id
    }

    #[track_caller]
    fn grant(
        storage: &Clusterwide,
        privilege: PrivilegeType,
        object_type: SchemaObjectType,
        object_id: i64,
        grantee_id: UserId,
        grantor_id: Option<UserId>,
    ) {
        let priv_def = PrivilegeDef::new(
            privilege,
            object_type,
            object_id,
            grantee_id,
            grantor_id.unwrap_or(session::uid().unwrap()),
            0,
        )
        .expect("must be valid");

        access_check_op(
            storage,
            &Op::Acl(Acl::GrantPrivilege {
                priv_def: priv_def.clone(),
            }),
            priv_def.grantor_id(),
        )
        .unwrap();

        on_master_grant_privilege(&priv_def).unwrap();
    }

    #[track_caller]
    fn revoke(
        storage: &Clusterwide,
        grantee_id: UserId,
        privilege: PrivilegeType,
        object_type: SchemaObjectType,
        object_id: i64,
    ) {
        let priv_def = PrivilegeDef::new(
            privilege,
            object_type,
            object_id,
            grantee_id,
            session::uid().unwrap(),
            0,
        )
        .expect("must be valid");

        access_check_op(
            storage,
            &Op::Acl(Acl::RevokePrivilege {
                priv_def: priv_def.clone(),
                initiator: session::uid().unwrap(),
            }),
            priv_def.grantor_id(),
        )
        .unwrap();
        on_master_revoke_privilege(&priv_def).unwrap()
    }

    #[tarantool::test]
    fn validate_access_check_ddl() {
        let user_name = "box_access_check_space_test_user";

        let user_id = make_user(user_name, None);
        let storage = Clusterwide::for_tests();

        // space
        let space_name = "test_box_access_check_ddl";
        let space = Space::create(space_name, &SpaceCreateOptions::default()).unwrap();

        // create space
        {
            let space_to_be_created = Ddl::CreateTable {
                id: 42,
                name: String::from("space_to_be_created"),
                format: vec![],
                primary_key: vec![],
                distribution: Distribution::Global,
                engine: SpaceEngineType::Blackhole,
                owner: user_id,
            };

            let e = access_check_ddl(&space_to_be_created, user_id).unwrap_err();

            assert_eq!(
                e.to_string(),
                format!("tarantool error: AccessDenied: Create access to space 'space_to_be_created' is denied for user '{user_name}'"),
            );

            grant(
                &storage,
                PrivilegeType::Create,
                SchemaObjectType::Table,
                -1,
                user_id,
                None,
            );

            access_check_ddl(&space_to_be_created, user_id).unwrap();
        }

        // drop can be granted with wildcard, check on particular entity works
        {
            let e = access_check_ddl(
                &Ddl::DropTable {
                    id: space.id(),
                    initiator: user_id,
                },
                user_id,
            )
            .unwrap_err();

            assert_eq!(
                e.to_string(),
                format!("tarantool error: AccessDenied: Drop access to space '{space_name}' is denied for user '{user_name}'"),
            );

            grant(
                &storage,
                PrivilegeType::Drop,
                SchemaObjectType::Table,
                -1,
                user_id,
                None,
            );

            access_check_ddl(
                &Ddl::DropTable {
                    id: space.id(),
                    initiator: user_id,
                },
                user_id,
            )
            .unwrap();
        }

        revoke(
            &storage,
            user_id,
            PrivilegeType::Drop,
            SchemaObjectType::Table,
            -1,
        );

        // drop on particular entity works
        {
            let e = access_check_ddl(
                &Ddl::DropTable {
                    id: space.id(),
                    initiator: user_id,
                },
                user_id,
            )
            .unwrap_err();

            assert_eq!(
                e.to_string(),
                format!("tarantool error: AccessDenied: Drop access to space '{space_name}' is denied for user '{user_name}'"),
            );

            grant(
                &storage,
                PrivilegeType::Drop,
                SchemaObjectType::Table,
                space.id() as i64,
                user_id,
                None,
            );

            access_check_ddl(
                &Ddl::DropTable {
                    id: space.id(),
                    initiator: user_id,
                },
                user_id,
            )
            .unwrap();
        }

        // owner has privileges on the object
        // owner can grant permissions on the object to other users
        {
            let grantee_user_name = format!("{user_name}_grantee");
            let grantee_user_id = make_user(&grantee_user_name, None);

            let space_name_grant = format!("{space_name}_grant");
            let space_opts = SpaceCreateOptions {
                user: Some(user_name.into()),
                ..Default::default()
            };
            let space_grant = Space::create(&space_name_grant, &space_opts).unwrap();

            let drop_op = |initiator| Op::DdlPrepare {
                schema_version: 0,
                ddl: Ddl::DropTable {
                    id: space_grant.id(),
                    initiator,
                },
            };
            let write_op = |initiator| {
                Op::Dml(Dml::Insert {
                    table: space_grant.id(),
                    tuple: TupleBuffer::from(Tuple::new(&(1,)).unwrap()),
                    initiator,
                })
            };

            // owner himself has permission on an object
            for op in [drop_op(user_id), write_op(user_id)] {
                access_check_op(&storage, &op, user_id).unwrap();
            }

            // owner can grant permissions to another user
            for (privilege, privilege_name, op) in [
                (PrivilegeType::Drop, "Drop", drop_op(grantee_user_id)),
                (PrivilegeType::Write, "Write", write_op(grantee_user_id)),
            ] {
                // run access check for another user, it fails without grant
                let e = access_check_op(&storage, &op, grantee_user_id).unwrap_err();

                assert_eq!(
                    e.to_string(),
                    format!("tarantool error: AccessDenied: {privilege_name} access to space '{space_name_grant}' is denied for user '{grantee_user_name}'"),
                );

                // grant permission on behalf of the user owning the space
                grant(
                    &storage,
                    privilege,
                    SchemaObjectType::Table,
                    space_grant.id() as _,
                    grantee_user_id,
                    Some(user_id),
                );

                // access check should succeed
                access_check_op(&storage, &op, grantee_user_id).unwrap();
            }
        }
    }

    #[tarantool::test]
    fn validate_access_check_acl_user() {
        // create works with passed id
        let actor_user_name = "box_access_check_ddl_test_user_actor";
        let user_under_test_name = "box_access_check_ddl_test_user";

        let storage = Clusterwide::for_tests();

        let actor_user_id = make_user(actor_user_name, None);
        let user_under_test_id = make_user(user_under_test_name, None);

        // create works with passed id
        {
            let e = access_check_acl(
                &storage,
                &Acl::CreateUser {
                    user_def: dummy_user_def(
                        123,
                        String::from("user_to_be_created"),
                        Some(actor_user_id),
                    ),
                },
                actor_user_id,
            )
            .unwrap_err();

            assert_eq!(
                e.to_string(),
                format!("tarantool error: AccessDenied: Create access to user 'user_to_be_created' is denied for user '{actor_user_name}'"),
            );

            grant(
                &storage,
                PrivilegeType::Create,
                SchemaObjectType::User,
                -1,
                actor_user_id,
                None,
            );

            access_check_acl(
                &storage,
                &Acl::CreateUser {
                    user_def: dummy_user_def(
                        123,
                        String::from("user_to_be_created"),
                        Some(actor_user_id),
                    ),
                },
                actor_user_id,
            )
            .unwrap();
        }

        // drop can be granted with wildcard, check on particular entity works
        {
            let e = access_check_acl(
                &storage,
                &Acl::DropUser {
                    user_id: user_under_test_id,
                    initiator: actor_user_id,
                    schema_version: 0,
                },
                actor_user_id,
            )
            .unwrap_err();

            assert_eq!(
                e.to_string(),
                format!("tarantool error: AccessDenied: Drop access to user '{user_under_test_name}' is denied for user '{actor_user_name}'"),
            );

            grant(
                &storage,
                PrivilegeType::Drop,
                SchemaObjectType::User,
                -1,
                actor_user_id,
                None,
            );

            access_check_acl(
                &storage,
                &Acl::DropUser {
                    user_id: user_under_test_id,
                    initiator: actor_user_id,
                    schema_version: 0,
                },
                actor_user_id,
            )
            .unwrap();
        }

        // alter can be granted with wildcard, check on particular entity works
        {
            let e = access_check_acl(
                &storage,
                &Acl::ChangeAuth {
                    user_id: user_under_test_id,
                    auth: dummy_auth_def(),
                    initiator: actor_user_id,
                    schema_version: 0,
                },
                actor_user_id,
            )
            .unwrap_err();

            assert_eq!(
                e.to_string(),
                format!("tarantool error: AccessDenied: Alter access to user '{user_under_test_name}' is denied for user '{actor_user_name}'"),
            );

            grant(
                &storage,
                PrivilegeType::Alter,
                SchemaObjectType::User,
                -1,
                actor_user_id,
                None,
            );

            access_check_acl(
                &storage,
                &Acl::ChangeAuth {
                    user_id: user_under_test_id,
                    auth: dummy_auth_def(),
                    initiator: actor_user_id,
                    schema_version: 0,
                },
                actor_user_id,
            )
            .unwrap();
        }

        revoke(
            &storage,
            actor_user_id,
            PrivilegeType::Drop,
            SchemaObjectType::User,
            -1,
        );
        revoke(
            &storage,
            actor_user_id,
            PrivilegeType::Alter,
            SchemaObjectType::User,
            -1,
        );

        // drop on particular entity works
        {
            let e = access_check_acl(
                &storage,
                &Acl::DropUser {
                    user_id: user_under_test_id,
                    initiator: actor_user_id,
                    schema_version: 0,
                },
                actor_user_id,
            )
            .unwrap_err();

            assert_eq!(
                e.to_string(),
                format!("tarantool error: AccessDenied: Drop access to user '{user_under_test_name}' is denied for user '{actor_user_name}'"),
            );

            grant(
                &storage,
                PrivilegeType::Drop,
                SchemaObjectType::User,
                user_under_test_id as i64,
                actor_user_id,
                None,
            );

            access_check_acl(
                &storage,
                &Acl::DropUser {
                    user_id: user_under_test_id,
                    initiator: actor_user_id,
                    schema_version: 0,
                },
                actor_user_id,
            )
            .unwrap();
        }

        // alter on particular entity works
        {
            let e = access_check_acl(
                &storage,
                &Acl::ChangeAuth {
                    user_id: user_under_test_id,
                    auth: dummy_auth_def(),
                    initiator: actor_user_id,
                    schema_version: 0,
                },
                actor_user_id,
            )
            .unwrap_err();

            assert_eq!(
                e.to_string(),
                format!("tarantool error: AccessDenied: Alter access to user '{user_under_test_name}' is denied for user '{actor_user_name}'"),
            );

            grant(
                &storage,
                PrivilegeType::Alter,
                SchemaObjectType::User,
                user_under_test_id as i64,
                actor_user_id,
                None,
            );

            access_check_acl(
                &storage,
                &Acl::ChangeAuth {
                    user_id: user_under_test_id,
                    auth: dummy_auth_def(),
                    initiator: actor_user_id,
                    schema_version: 0,
                },
                actor_user_id,
            )
            .unwrap();
        }

        // owner has privileges on the object
        // owner can grant permissions on the object to other users
        {
            let grantee_user_name = format!("{actor_user_name}_grantee");
            let grantee_user_id = make_user(&grantee_user_name, None);

            let user_name_grant = format!("{actor_user_name}_grant");
            let user_id_grant = make_user(&user_name_grant, Some(actor_user_id));

            let drop_op = |initiator| {
                Op::Acl(Acl::DropUser {
                    user_id: user_id_grant,
                    initiator,
                    schema_version: 0,
                })
            };

            let alter_op = |initiator| {
                Op::Acl(Acl::ChangeAuth {
                    user_id: user_id_grant,
                    auth: dummy_auth_def(),
                    initiator,
                    schema_version: 0,
                })
            };

            // owner himself has permission on an object
            for op in [drop_op(actor_user_id), alter_op(actor_user_id)] {
                access_check_op(&storage, &op, actor_user_id).unwrap();
            }

            // owner can grant it to another user
            for (privilege, privilege_name, op) in [
                (PrivilegeType::Drop, "Drop", drop_op(grantee_user_id)),
                (PrivilegeType::Alter, "Alter", alter_op(grantee_user_id)),
            ] {
                // run access check for another user, it fails without grant
                let e = access_check_op(&storage, &op, grantee_user_id).unwrap_err();

                assert_eq!(
                    e.to_string(),
                    format!("tarantool error: AccessDenied: {privilege_name} access to user '{user_name_grant}' is denied for user '{grantee_user_name}'"),
                );

                // grant permission on behalf of the user owning the user
                grant(
                    &storage,
                    privilege,
                    SchemaObjectType::User,
                    user_id_grant as _,
                    grantee_user_id,
                    Some(actor_user_id),
                );

                // access check should succeed
                access_check_op(&storage, &op, grantee_user_id).unwrap();
            }
        }
    }

    #[tarantool::test]
    fn validate_access_check_acl_role() {
        let user_name = "box_access_check_ddl_test_role";

        let user_id = make_user(user_name, None);
        let storage = Clusterwide::for_tests();

        let role_name = "box_access_check_ddl_test_role_some_role";
        let role_def = RoleDef {
            id: next_user_id(),
            name: String::from(role_name),
            schema_version: 0,
            owner: ADMIN_ID,
        };
        on_master_create_role(&role_def).expect("create role shouldnt fail");

        // create works with passed id
        {
            let role_to_be_created = RoleDef {
                id: 123,
                name: String::from("role_to_be_created"),
                schema_version: 0,
                owner: user_id,
            };

            let e = access_check_acl(
                &storage,
                &Acl::CreateRole {
                    role_def: role_to_be_created.clone(),
                },
                user_id,
            )
            .unwrap_err();

            assert_eq!(
                e.to_string(),
                format!("tarantool error: AccessDenied: Create access to role 'role_to_be_created' is denied for user '{user_name}'"),
            );

            grant(
                &storage,
                PrivilegeType::Create,
                SchemaObjectType::Role,
                -1,
                user_id,
                None,
            );

            access_check_acl(
                &storage,
                &Acl::CreateRole {
                    role_def: role_to_be_created,
                },
                user_id,
            )
            .unwrap();
        }

        // drop can be granted with wildcard, check on particular entity works
        {
            let e = access_check_acl(
                &storage,
                &Acl::DropRole {
                    role_id: role_def.id,
                    initiator: user_id,
                    schema_version: 0,
                },
                user_id,
            )
            .unwrap_err();

            assert_eq!(
                e.to_string(),
                format!("tarantool error: AccessDenied: Drop access to role '{role_name}' is denied for user '{user_name}'"),
            );

            grant(
                &storage,
                PrivilegeType::Drop,
                SchemaObjectType::Role,
                -1,
                user_id,
                None,
            );

            access_check_acl(
                &storage,
                &Acl::DropRole {
                    role_id: role_def.id,
                    initiator: user_id,
                    schema_version: 0,
                },
                user_id,
            )
            .unwrap();
        }

        revoke(
            &storage,
            user_id,
            PrivilegeType::Drop,
            SchemaObjectType::Role,
            -1,
        );

        // drop on particular entity works
        {
            let e = access_check_acl(
                &storage,
                &Acl::DropRole {
                    role_id: role_def.id,
                    initiator: user_id,
                    schema_version: 0,
                },
                user_id,
            )
            .unwrap_err();

            assert_eq!(
                e.to_string(),
                format!("tarantool error: AccessDenied: Drop access to role '{role_name}' is denied for user '{user_name}'"),
            );

            grant(
                &storage,
                PrivilegeType::Drop,
                SchemaObjectType::Role,
                role_def.id as i64,
                user_id,
                None,
            );

            access_check_acl(
                &storage,
                &Acl::DropRole {
                    role_id: role_def.id,
                    initiator: user_id,
                    schema_version: 0,
                },
                user_id,
            )
            .unwrap();
        }

        // owner has privileges on the object
        // owner can grant permissions on the object to other users
        {
            let grantee_user_name = format!("{user_name}_grantee");
            let grantee_user_id = make_user(&grantee_user_name, None);

            let role_name_grant = format!("{role_name}_grant");
            let role_id_grant = next_user_id();
            let role_def = RoleDef {
                id: role_id_grant,
                name: role_name_grant.clone(),
                schema_version: 0,
                owner: user_id,
            };
            on_master_create_role(&role_def).expect("create role shouldn't fail");

            let op = |initiator| {
                Op::Acl(Acl::DropRole {
                    role_id: role_id_grant,
                    initiator,
                    schema_version: 0,
                })
            };

            // owner himself has permission on an object
            access_check_op(&storage, &op(user_id), user_id).unwrap();

            // run access check for another user, it fails without grant
            let e = access_check_op(&storage, &op(grantee_user_id), grantee_user_id).unwrap_err();

            assert_eq!(
                    e.to_string(),
                    format!("tarantool error: AccessDenied: Drop access to role '{role_name_grant}' is denied for user '{grantee_user_name}'"),
                );

            // grant permission on behalf of the user owning the role
            grant(
                &storage,
                PrivilegeType::Drop,
                SchemaObjectType::Role,
                role_id_grant as _,
                grantee_user_id,
                Some(user_id),
            );

            // access check should succeed
            access_check_op(&storage, &op(grantee_user_id), grantee_user_id).unwrap();
        }
    }

    #[tarantool::test]
    fn prohibit_circular_role_grant() {
        let storage = Clusterwide::for_tests();

        let create_role = |name| {
            let id = next_user_id();
            let role_def = RoleDef {
                id,
                name: String::from(name),
                schema_version: 0,
                owner: ADMIN_ID,
            };

            on_master_create_role(&role_def).expect("create role shouldn't fail");
            global_create_role(&storage, &role_def).expect("create role shouldn't fail");
            id
        };

        let parent_id = create_role("Parent");
        let child_id = create_role("Child");

        // circular grant: child to parent, parent to child should throw error
        {
            grant(
                &storage,
                PrivilegeType::Execute,
                SchemaObjectType::Role,
                child_id as i64,
                parent_id,
                None,
            );

            // grant child to parent
            let privilege = PrivilegeDef::new(
                PrivilegeType::Execute,
                SchemaObjectType::Role,
                child_id as i64,
                parent_id,
                ADMIN_ID,
                0,
            )
            .unwrap();

            global_grant_privilege(&storage, &privilege).unwrap();

            // grant parent to child
            let privilege = PrivilegeDef::new(
                PrivilegeType::Execute,
                SchemaObjectType::Role,
                parent_id as i64,
                child_id,
                ADMIN_ID,
                0,
            )
            .unwrap();

            let e = access_check_acl(
                &storage,
                &Acl::GrantPrivilege {
                    priv_def: privilege,
                },
                ADMIN_ID,
            )
            .unwrap_err();

            assert_eq!(
                e.to_string(),
                format!("tarantool error: RoleLoop: Granting role Parent to role Child would create a loop"),
            );
        }
    }
}
