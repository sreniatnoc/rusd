use std::sync::Arc;
use tonic::{Code, Request, Response, Status};

use crate::cluster::ClusterManager;
use crate::etcdserverpb::cluster_server::Cluster;
use crate::etcdserverpb::*;
use crate::raft::node::RaftNode;

pub struct ClusterService {
    raft: Arc<RaftNode>,
    cluster_mgr: Arc<ClusterManager>,
}

impl ClusterService {
    pub fn new(raft: Arc<RaftNode>, cluster_mgr: Arc<ClusterManager>) -> Self {
        Self { raft, cluster_mgr }
    }

    fn build_response_header(&self) -> ResponseHeader {
        ResponseHeader {
            cluster_id: self.raft.cluster_id(),
            member_id: self.raft.member_id(),
            revision: 0,
            raft_term: self.raft.current_term(),
        }
    }

    fn members_to_proto(&self) -> Vec<Member> {
        self.cluster_mgr
            .list_members()
            .into_iter()
            .map(|m| Member {
                id: m.id,
                name: m.name.clone(),
                peer_ur_ls: m.peer_urls.clone(),
                client_ur_ls: m.client_urls.clone(),
                is_learner: m.is_learner,
            })
            .collect()
    }
}

#[tonic::async_trait]
impl Cluster for ClusterService {
    async fn member_add(
        &self,
        request: Request<MemberAddRequest>,
    ) -> Result<Response<MemberAddResponse>, Status> {
        let req = request.into_inner();

        if !self.raft.is_leader() {
            return Err(Status::new(Code::FailedPrecondition, "not a leader"));
        }

        if req.peer_ur_ls.is_empty() {
            return Err(Status::new(
                Code::InvalidArgument,
                "peer URLs must not be empty",
            ));
        }

        // Generate a name from the peer URL for the new member
        let member_name = format!("member-{}", self.cluster_mgr.member_count() + 1);

        // Add member to cluster manager
        let new_member = self
            .cluster_mgr
            .add_member(
                member_name,
                req.peer_ur_ls.clone(),
                vec![], // client URLs will be set later via member_update
                req.is_learner,
            )
            .map_err(|e| Status::new(Code::Internal, format!("add member failed: {}", e)))?;

        // Propose configuration change to Raft
        let add_cmd = format!(
            "ADD_MEMBER:{}:{}:{}",
            new_member.id,
            req.peer_ur_ls.join(","),
            req.is_learner
        );
        self.raft
            .propose_conf_change(add_cmd.into_bytes())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("conf change failed: {}", e)))?;

        let proto_member = Member {
            id: new_member.id,
            name: new_member.name.clone(),
            peer_ur_ls: new_member.peer_urls.clone(),
            client_ur_ls: new_member.client_urls.clone(),
            is_learner: new_member.is_learner,
        };

        let response = MemberAddResponse {
            header: Some(self.build_response_header()),
            member: Some(proto_member),
            members: self.members_to_proto(),
        };

        Ok(Response::new(response))
    }

    async fn member_remove(
        &self,
        request: Request<MemberRemoveRequest>,
    ) -> Result<Response<MemberRemoveResponse>, Status> {
        let req = request.into_inner();

        if req.id == 0 {
            return Err(Status::new(
                Code::InvalidArgument,
                "member id must not be zero",
            ));
        }

        if !self.raft.is_leader() {
            return Err(Status::new(Code::FailedPrecondition, "not a leader"));
        }

        // Remove from cluster manager
        self.cluster_mgr
            .remove_member(req.id)
            .map_err(|e| Status::new(Code::Internal, format!("remove member failed: {}", e)))?;

        // Propose configuration change to Raft
        let remove_cmd = format!("REMOVE_MEMBER:{}", req.id);
        self.raft
            .propose_conf_change(remove_cmd.into_bytes())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("conf change failed: {}", e)))?;

        let response = MemberRemoveResponse {
            header: Some(self.build_response_header()),
            members: self.members_to_proto(),
        };

        Ok(Response::new(response))
    }

    async fn member_update(
        &self,
        request: Request<MemberUpdateRequest>,
    ) -> Result<Response<MemberUpdateResponse>, Status> {
        let req = request.into_inner();

        if req.id == 0 {
            return Err(Status::new(
                Code::InvalidArgument,
                "member id must not be zero",
            ));
        }

        if !self.raft.is_leader() {
            return Err(Status::new(Code::FailedPrecondition, "not a leader"));
        }

        // Update client URLs in cluster manager
        self.cluster_mgr
            .update_member(req.id, req.client_ur_ls.clone())
            .map_err(|e| Status::new(Code::Internal, format!("update member failed: {}", e)))?;

        // Propose configuration change to Raft
        let update_cmd = format!("UPDATE_MEMBER:{}:{}", req.id, req.client_ur_ls.join(","));
        self.raft
            .propose_conf_change(update_cmd.into_bytes())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("conf change failed: {}", e)))?;

        let response = MemberUpdateResponse {
            header: Some(self.build_response_header()),
            members: self.members_to_proto(),
        };

        Ok(Response::new(response))
    }

    async fn member_list(
        &self,
        _request: Request<MemberListRequest>,
    ) -> Result<Response<MemberListResponse>, Status> {
        let response = MemberListResponse {
            header: Some(self.build_response_header()),
            members: self.members_to_proto(),
        };

        Ok(Response::new(response))
    }

    async fn member_promote(
        &self,
        request: Request<MemberPromoteRequest>,
    ) -> Result<Response<MemberPromoteResponse>, Status> {
        let req = request.into_inner();

        if req.id == 0 {
            return Err(Status::new(
                Code::InvalidArgument,
                "member id must not be zero",
            ));
        }

        if !self.raft.is_leader() {
            return Err(Status::new(Code::FailedPrecondition, "not a leader"));
        }

        // Promote learner to voter in cluster manager
        self.cluster_mgr
            .promote_member(req.id)
            .map_err(|e| Status::new(Code::Internal, format!("promote member failed: {}", e)))?;

        // Propose configuration change to Raft
        let promote_cmd = format!("PROMOTE_MEMBER:{}", req.id);
        self.raft
            .propose_conf_change(promote_cmd.into_bytes())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("conf change failed: {}", e)))?;

        let response = MemberPromoteResponse {
            header: Some(self.build_response_header()),
            members: self.members_to_proto(),
        };

        Ok(Response::new(response))
    }
}
