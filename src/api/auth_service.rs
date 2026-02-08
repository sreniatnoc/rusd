use tonic::{Request, Response, Status, Code};
use std::sync::Arc;

use crate::etcdserverpb::*;
use crate::etcdserverpb::auth_server::Auth;
use crate::raft::node::RaftNode;

/// Bridge between the server layer and the auth API.
pub struct ApiAuthManager {
    pub raft: Arc<RaftNode>,
    pub store: Arc<crate::auth::AuthStore>,
}

impl ApiAuthManager {
    pub fn new(raft: Arc<RaftNode>, store: Arc<crate::auth::AuthStore>) -> Self {
        Self { raft, store }
    }

    pub fn build_response_header(&self) -> ResponseHeader {
        ResponseHeader {
            cluster_id: 0,
            member_id: 0,
            revision: 0,
            raft_term: self.raft.current_term(),
        }
    }

    pub fn is_leader(&self) -> bool {
        self.raft.is_leader()
    }
}

pub struct AuthManager {
    raft: Arc<RaftNode>,
}

impl AuthManager {
    pub fn new(raft: Arc<RaftNode>) -> Self {
        Self { raft }
    }

    fn build_response_header(&self) -> ResponseHeader {
        ResponseHeader {
            cluster_id: self.raft.cluster_id(),
            member_id: self.raft.member_id(),
            revision: 0,
            raft_term: self.raft.current_term(),
        }
    }
}

pub struct AuthService {
    auth_mgr: Arc<AuthManager>,
}

impl AuthService {
    pub fn new(auth_mgr: Arc<AuthManager>) -> Self {
        Self { auth_mgr }
    }
}

#[tonic::async_trait]
impl Auth for AuthService {
    async fn auth_enable(
        &self,
        _request: Request<AuthEnableRequest>,
    ) -> Result<Response<AuthEnableResponse>, Status> {
        // Check if leader
        if !self.auth_mgr.raft.is_leader() {
            return Err(Status::new(
                Code::FailedPrecondition,
                "not a leader",
            ));
        }

        // Propose to Raft
        let enable_cmd = "ENABLE_AUTH";

        self.auth_mgr.raft.propose(enable_cmd.as_bytes().to_vec())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("enable auth failed: {}", e)))?;

        let response = AuthEnableResponse {
            header: Some(self.auth_mgr.build_response_header()),
        };

        Ok(Response::new(response))
    }

    async fn auth_disable(
        &self,
        _request: Request<AuthDisableRequest>,
    ) -> Result<Response<AuthDisableResponse>, Status> {
        // Check if leader
        if !self.auth_mgr.raft.is_leader() {
            return Err(Status::new(
                Code::FailedPrecondition,
                "not a leader",
            ));
        }

        // Propose to Raft
        let disable_cmd = "DISABLE_AUTH";

        self.auth_mgr.raft.propose(disable_cmd.as_bytes().to_vec())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("disable auth failed: {}", e)))?;

        let response = AuthDisableResponse {
            header: Some(self.auth_mgr.build_response_header()),
        };

        Ok(Response::new(response))
    }

    async fn auth_status(
        &self,
        _request: Request<AuthStatusRequest>,
    ) -> Result<Response<AuthStatusResponse>, Status> {
        let response = AuthStatusResponse {
            header: Some(self.auth_mgr.build_response_header()),
            enabled: true,
            auth_revision: 0,
        };
        Ok(Response::new(response))
    }

    async fn authenticate(
        &self,
        request: Request<AuthenticateRequest>,
    ) -> Result<Response<AuthenticateResponse>, Status> {
        let req = request.into_inner();

        // Validate input
        if req.name.is_empty() || req.password.is_empty() {
            return Err(Status::new(
                Code::InvalidArgument,
                "name and password must not be empty",
            ));
        }

        // TODO: Verify credentials against auth store
        // For now, return a dummy token
        let token = format!("token-{}-{}", req.name, req.password);
        let response = AuthenticateResponse {
            header: Some(self.auth_mgr.build_response_header()),
            token,
        };
        Ok(Response::new(response))
    }

    async fn user_add(
        &self,
        request: Request<AuthUserAddRequest>,
    ) -> Result<Response<AuthUserAddResponse>, Status> {
        let req = request.into_inner();

        // Validate input
        if req.name.is_empty() {
            return Err(Status::new(
                Code::InvalidArgument,
                "user name must not be empty",
            ));
        }

        if req.password.is_empty() {
            return Err(Status::new(
                Code::InvalidArgument,
                "password must not be empty",
            ));
        }

        // Check if leader
        if !self.auth_mgr.raft.is_leader() {
            return Err(Status::new(
                Code::FailedPrecondition,
                "not a leader",
            ));
        }

        // Propose to Raft
        let add_cmd = format!("ADD_USER:{}:{}", req.name, req.password);

        self.auth_mgr.raft.propose(add_cmd.into_bytes())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("add user failed: {}", e)))?;

        let response = AuthUserAddResponse {
            header: Some(self.auth_mgr.build_response_header()),
        };

        Ok(Response::new(response))
    }

    async fn user_delete(
        &self,
        request: Request<AuthUserDeleteRequest>,
    ) -> Result<Response<AuthUserDeleteResponse>, Status> {
        let req = request.into_inner();

        // Validate input
        if req.name.is_empty() {
            return Err(Status::new(
                Code::InvalidArgument,
                "user name must not be empty",
            ));
        }

        // Check if leader
        if !self.auth_mgr.raft.is_leader() {
            return Err(Status::new(
                Code::FailedPrecondition,
                "not a leader",
            ));
        }

        // Propose to Raft
        let delete_cmd = format!("DELETE_USER:{}", req.name);

        self.auth_mgr.raft.propose(delete_cmd.into_bytes())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("delete user failed: {}", e)))?;

        let response = AuthUserDeleteResponse {
            header: Some(self.auth_mgr.build_response_header()),
        };

        Ok(Response::new(response))
    }

    async fn user_get(
        &self,
        request: Request<AuthUserGetRequest>,
    ) -> Result<Response<AuthUserGetResponse>, Status> {
        let req = request.into_inner();

        // Validate input
        if req.name.is_empty() {
            return Err(Status::new(
                Code::InvalidArgument,
                "user name must not be empty",
            ));
        }

        // TODO: Get user details from auth store
        let response = AuthUserGetResponse {
            header: Some(self.auth_mgr.build_response_header()),
            roles: vec![],
        };
        Ok(Response::new(response))
    }

    async fn user_list(
        &self,
        _request: Request<AuthUserListRequest>,
    ) -> Result<Response<AuthUserListResponse>, Status> {
        // TODO: Get all users from auth store
        let response = AuthUserListResponse {
            header: Some(self.auth_mgr.build_response_header()),
            users: vec![],
        };

        Ok(Response::new(response))
    }

    async fn user_change_password(
        &self,
        request: Request<AuthUserChangePasswordRequest>,
    ) -> Result<Response<AuthUserChangePasswordResponse>, Status> {
        let req = request.into_inner();

        // Validate input
        if req.name.is_empty() {
            return Err(Status::new(
                Code::InvalidArgument,
                "user name must not be empty",
            ));
        }

        if req.password.is_empty() {
            return Err(Status::new(
                Code::InvalidArgument,
                "password must not be empty",
            ));
        }

        // Check if leader
        if !self.auth_mgr.raft.is_leader() {
            return Err(Status::new(
                Code::FailedPrecondition,
                "not a leader",
            ));
        }

        // Propose to Raft
        let change_cmd = format!("CHANGE_PASSWORD:{}:{}", req.name, req.password);

        self.auth_mgr.raft.propose(change_cmd.into_bytes())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("change password failed: {}", e)))?;

        let response = AuthUserChangePasswordResponse {
            header: Some(self.auth_mgr.build_response_header()),
        };

        Ok(Response::new(response))
    }

    async fn user_grant_role(
        &self,
        request: Request<AuthUserGrantRoleRequest>,
    ) -> Result<Response<AuthUserGrantRoleResponse>, Status> {
        let req = request.into_inner();

        // Validate input
        if req.user.is_empty() {
            return Err(Status::new(
                Code::InvalidArgument,
                "user name must not be empty",
            ));
        }

        if req.role.is_empty() {
            return Err(Status::new(
                Code::InvalidArgument,
                "role name must not be empty",
            ));
        }

        // Check if leader
        if !self.auth_mgr.raft.is_leader() {
            return Err(Status::new(
                Code::FailedPrecondition,
                "not a leader",
            ));
        }

        // Propose to Raft
        let grant_cmd = format!("GRANT_ROLE:{}:{}", req.user, req.role);

        self.auth_mgr.raft.propose(grant_cmd.into_bytes())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("grant role failed: {}", e)))?;

        let response = AuthUserGrantRoleResponse {
            header: Some(self.auth_mgr.build_response_header()),
        };

        Ok(Response::new(response))
    }

    async fn user_revoke_role(
        &self,
        request: Request<AuthUserRevokeRoleRequest>,
    ) -> Result<Response<AuthUserRevokeRoleResponse>, Status> {
        let req = request.into_inner();

        // Validate input
        if req.name.is_empty() {
            return Err(Status::new(
                Code::InvalidArgument,
                "user name must not be empty",
            ));
        }

        if req.role.is_empty() {
            return Err(Status::new(
                Code::InvalidArgument,
                "role name must not be empty",
            ));
        }

        // Check if leader
        if !self.auth_mgr.raft.is_leader() {
            return Err(Status::new(
                Code::FailedPrecondition,
                "not a leader",
            ));
        }

        // Propose to Raft
        let revoke_cmd = format!("REVOKE_ROLE:{}:{}", req.name, req.role);

        self.auth_mgr.raft.propose(revoke_cmd.into_bytes())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("revoke role failed: {}", e)))?;

        let response = AuthUserRevokeRoleResponse {
            header: Some(self.auth_mgr.build_response_header()),
        };

        Ok(Response::new(response))
    }

    async fn role_add(
        &self,
        request: Request<AuthRoleAddRequest>,
    ) -> Result<Response<AuthRoleAddResponse>, Status> {
        let req = request.into_inner();

        // Validate input
        if req.name.is_empty() {
            return Err(Status::new(
                Code::InvalidArgument,
                "role name must not be empty",
            ));
        }

        // Check if leader
        if !self.auth_mgr.raft.is_leader() {
            return Err(Status::new(
                Code::FailedPrecondition,
                "not a leader",
            ));
        }

        // Propose to Raft
        let add_cmd = format!("ADD_ROLE:{}", req.name);

        self.auth_mgr.raft.propose(add_cmd.into_bytes())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("add role failed: {}", e)))?;

        let response = AuthRoleAddResponse {
            header: Some(self.auth_mgr.build_response_header()),
        };

        Ok(Response::new(response))
    }

    async fn role_delete(
        &self,
        request: Request<AuthRoleDeleteRequest>,
    ) -> Result<Response<AuthRoleDeleteResponse>, Status> {
        let req = request.into_inner();

        // Validate input
        if req.role.is_empty() {
            return Err(Status::new(
                Code::InvalidArgument,
                "role name must not be empty",
            ));
        }

        // Check if leader
        if !self.auth_mgr.raft.is_leader() {
            return Err(Status::new(
                Code::FailedPrecondition,
                "not a leader",
            ));
        }

        // Propose to Raft
        let delete_cmd = format!("DELETE_ROLE:{}", req.role);

        self.auth_mgr.raft.propose(delete_cmd.into_bytes())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("delete role failed: {}", e)))?;

        let response = AuthRoleDeleteResponse {
            header: Some(self.auth_mgr.build_response_header()),
        };

        Ok(Response::new(response))
    }

    async fn role_get(
        &self,
        request: Request<AuthRoleGetRequest>,
    ) -> Result<Response<AuthRoleGetResponse>, Status> {
        let req = request.into_inner();

        // Validate input
        if req.role.is_empty() {
            return Err(Status::new(
                Code::InvalidArgument,
                "role name must not be empty",
            ));
        }

        // TODO: Get role details from auth store
        // AuthRoleGetResponse only has perm field, not kv/rev
        let response = AuthRoleGetResponse {
            header: Some(self.auth_mgr.build_response_header()),
            perm: vec![],
        };
        Ok(Response::new(response))
    }

    async fn role_list(
        &self,
        _request: Request<AuthRoleListRequest>,
    ) -> Result<Response<AuthRoleListResponse>, Status> {
        // TODO: Get all roles from auth store
        let response = AuthRoleListResponse {
            header: Some(self.auth_mgr.build_response_header()),
            roles: vec![],
        };

        Ok(Response::new(response))
    }

    async fn role_grant_permission(
        &self,
        request: Request<AuthRoleGrantPermissionRequest>,
    ) -> Result<Response<AuthRoleGrantPermissionResponse>, Status> {
        let req = request.into_inner();

        // Validate input
        if req.name.is_empty() {
            return Err(Status::new(
                Code::InvalidArgument,
                "role name must not be empty",
            ));
        }

        if req.perm.is_none() {
            return Err(Status::new(
                Code::InvalidArgument,
                "permission must be provided",
            ));
        }

        // Check if leader
        if !self.auth_mgr.raft.is_leader() {
            return Err(Status::new(
                Code::FailedPrecondition,
                "not a leader",
            ));
        }

        // Propose to Raft
        // TODO: Serialize permission using proto encoding instead of serde_json
        let grant_cmd = format!("GRANT_PERMISSION:{}", req.name);

        self.auth_mgr.raft.propose(grant_cmd.into_bytes())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("grant permission failed: {}", e)))?;

        let response = AuthRoleGrantPermissionResponse {
            header: Some(self.auth_mgr.build_response_header()),
        };

        Ok(Response::new(response))
    }

    async fn role_revoke_permission(
        &self,
        request: Request<AuthRoleRevokePermissionRequest>,
    ) -> Result<Response<AuthRoleRevokePermissionResponse>, Status> {
        let req = request.into_inner();

        // Validate input
        if req.role.is_empty() {
            return Err(Status::new(
                Code::InvalidArgument,
                "role name must not be empty",
            ));
        }

        // Check if leader
        if !self.auth_mgr.raft.is_leader() {
            return Err(Status::new(
                Code::FailedPrecondition,
                "not a leader",
            ));
        }

        // Propose to Raft
        // TODO: Serialize key/range_end using proto encoding
        let revoke_cmd = format!("REVOKE_PERMISSION:{}", req.role);

        self.auth_mgr.raft.propose(revoke_cmd.into_bytes())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("revoke permission failed: {}", e)))?;

        let response = AuthRoleRevokePermissionResponse {
            header: Some(self.auth_mgr.build_response_header()),
        };

        Ok(Response::new(response))
    }
}
