use std::sync::Arc;
use tonic::{Code, Request, Response, Status};

use crate::etcdserverpb::cluster_server::Cluster;
use crate::etcdserverpb::*;
use crate::raft::node::RaftNode;

pub struct ClusterService {
    raft: Arc<RaftNode>,
    members: Arc<parking_lot::RwLock<Vec<Member>>>,
}

impl ClusterService {
    pub fn new(raft: Arc<RaftNode>, members: Arc<parking_lot::RwLock<Vec<Member>>>) -> Self {
        Self { raft, members }
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

#[tonic::async_trait]
impl Cluster for ClusterService {
    async fn member_add(
        &self,
        request: Request<MemberAddRequest>,
    ) -> Result<Response<MemberAddResponse>, Status> {
        let req = request.into_inner();

        // Check if leader
        if !self.raft.is_leader() {
            return Err(Status::new(Code::FailedPrecondition, "not a leader"));
        }

        // TODO: Validate peer_urls from proto
        // Generate new member ID
        let member_id = 1u64; // TODO: Use actual member ID generation

        // Create new member with minimal fields
        let new_member = Member {
            id: member_id,
            ..Default::default()
        };

        // Propose configuration change to Raft
        let add_cmd = format!("ADD_MEMBER:{}", member_id);

        self.raft
            .propose_conf_change(add_cmd.into_bytes())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("conf change failed: {}", e)))?;

        // Add to members list
        {
            let mut members = self.members.write();
            members.push(new_member.clone());
        }

        // Get all members
        let members = self.members.read().clone();

        let response = MemberAddResponse {
            header: Some(self.build_response_header()),
            member: Some(new_member),
            members,
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

        // Check if leader
        if !self.raft.is_leader() {
            return Err(Status::new(Code::FailedPrecondition, "not a leader"));
        }

        // Propose configuration change to Raft
        let remove_cmd = format!("REMOVE_MEMBER:{}", req.id);

        self.raft
            .propose_conf_change(remove_cmd.into_bytes())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("conf change failed: {}", e)))?;

        // Remove from members list
        {
            let mut members = self.members.write();
            members.retain(|m| m.id != req.id);
        }

        // Get remaining members
        let members = self.members.read().clone();

        let response = MemberRemoveResponse {
            header: Some(self.build_response_header()),
            members,
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

        // Check if leader
        if !self.raft.is_leader() {
            return Err(Status::new(Code::FailedPrecondition, "not a leader"));
        }

        // Find and update member (if it exists)
        {
            let mut members = self.members.write();
            if let Some(_member) = members.iter_mut().find(|m| m.id == req.id) {
                // TODO: Update member client URLs from proto if field exists
                // member.client_urls = req.client_urls.clone();
            } else {
                return Err(Status::new(Code::NotFound, "member not found"));
            }
        }

        // Propose configuration change to Raft
        let update_cmd = format!("UPDATE_MEMBER:{}", req.id);

        self.raft
            .propose_conf_change(update_cmd.into_bytes())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("conf change failed: {}", e)))?;

        // Get all members
        let members = self.members.read().clone();

        let response = MemberUpdateResponse {
            header: Some(self.build_response_header()),
            members,
        };

        Ok(Response::new(response))
    }

    async fn member_list(
        &self,
        _request: Request<MemberListRequest>,
    ) -> Result<Response<MemberListResponse>, Status> {
        let members = self.members.read().clone();

        let response = MemberListResponse {
            header: Some(self.build_response_header()),
            members,
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

        // Check if leader
        if !self.raft.is_leader() {
            return Err(Status::new(Code::FailedPrecondition, "not a leader"));
        }

        // Find and promote member
        {
            let mut members = self.members.write();
            if let Some(member) = members.iter_mut().find(|m| m.id == req.id) {
                if member.is_learner {
                    member.is_learner = false;
                } else {
                    return Err(Status::new(
                        Code::FailedPrecondition,
                        "member is already a voting member",
                    ));
                }
            } else {
                return Err(Status::new(Code::NotFound, "member not found"));
            }
        }

        // Propose configuration change to Raft
        let promote_cmd = format!("PROMOTE_MEMBER:{}", req.id);

        self.raft
            .propose_conf_change(promote_cmd.into_bytes())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("conf change failed: {}", e)))?;

        // Get all members
        let members = self.members.read().clone();

        let response = MemberPromoteResponse {
            header: Some(self.build_response_header()),
            members,
        };

        Ok(Response::new(response))
    }
}
