use crate::*;

/// Changes whenever this fork's messages or fields change in a way that an
/// older fork build would misread, such as a renumbering. It is part of
/// the remote server binary's name, so that a client never reuses a server
/// from another revision that happens to share Zed's version, and both ends
/// compare it before anything else is exchanged (`CheckWireRevision`).
///
/// 2: `CheckWireRevision`, and `AddWorktree.code_runs_without_ide_decision`
/// in place of `code_execution_follows_ide`.
pub const JP_ENHANCE_WIRE_REVISION: u32 = 2;

crate::request_messages!(
    (CheckWireRevision, CheckWireRevisionResponse),
    (RescanProjectWorktree, RescanProjectWorktreeResponse),
    (SetWorktreeCodeExecution, Ack),
    (
        RequestProjectLaunchApproval,
        RequestProjectLaunchApprovalResponse
    ),
    (ProjectLaunchApprovalDecided, Ack),
    (ProjectLaunchApprovalRevoked, Ack),
    (WithdrawProjectLaunchApproval, Ack),
    (SaveBufferVerified, BufferSaved),
    (VerifyAgentRead, Ack),
    (ListSandboxProcesses, ListSandboxProcessesResponse),
    (StopSandboxProcesses, StopSandboxProcessesResponse),
    (SetEgressAllowlist, Ack),
    (ProxyHttpRequest, Ack),
    (ProxyHttpResponse, Ack),
    (RunTransferCommand, RunTransferCommandResponse),
    (CancelTransferCommand, Ack),
    (KillTerminalSession, Ack),
    (TransferAskPassRequest, TransferAskPassResponse),
    (ListTransferDirectory, ListTransferDirectoryResponse),
    (TransferFileOperation, Ack),
    (CheckDevTools, CheckDevToolsResponse),
    (InstallDevTool, InstallDevToolResponse),
    (GitRunCommand, GitRunCommandResponse),
    (GetClangdMemoryUsage, GetClangdMemoryUsageResponse),
    (GetSymbolIndexStatus, GetSymbolIndexStatusResponse),
    (SymbolIndexDefinitions, SymbolIndexDefinitionsResponse),
    (SymbolIndexCallers, SymbolIndexCallersResponse),
    (SymbolIndexCallees, SymbolIndexCalleesResponse),
    (SymbolIndexAssignments, SymbolIndexAssignmentsResponse),
    (SymbolIndexSearch, SymbolIndexSearchResponse),
    (RebuildSymbolIndex, Ack),
    (SymbolIndexDescribeType, SymbolIndexDescribeTypeResponse),
    (SymbolIndexResolveType, SymbolIndexResolveTypeResponse),
    (SymbolIndexExpandMember, SymbolIndexExpandMemberResponse),
    (SymbolIndexTypeTree, SymbolIndexTypeTreeResponse),
);

crate::entity_messages!(
    {project_id, ShareProject},
    RescanProjectWorktree,
    RequestProjectLaunchApproval,
    ProjectLaunchApprovalDecided,
    ProjectLaunchApprovalRevoked,
    WithdrawProjectLaunchApproval,
    SetWorktreeCodeExecution,
    SaveBufferVerified,
    VerifyAgentRead,
    ListSandboxProcesses,
    StopSandboxProcesses,
    RunTransferCommand,
    CancelTransferCommand,
    KillTerminalSession,
    TransferAskPassRequest,
    ListTransferDirectory,
    TransferFileOperation,
    CheckDevTools,
    InstallDevTool,
    GitRunCommand,
    CancelSearchCommits,
    GetClangdMemoryUsage,
    ClangdMemoryLimitExceeded,
    GetSymbolIndexStatus,
    UpdateSymbolIndexStatus,
    SymbolIndexDefinitions,
    SymbolIndexCallers,
    SymbolIndexCallees,
    SymbolIndexAssignments,
    SymbolIndexSearch,
    RebuildSymbolIndex,
    SymbolIndexDescribeType,
    SymbolIndexResolveType,
    SymbolIndexExpandMember,
    SymbolIndexTypeTree,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_egress_allowlist_round_trips_the_child_process_proxy_flag() {
        // ADR-F14: the flag rides along with the allowlist the IDE pushes to a
        // remote host. A client that predates the field has to decode as
        // absent rather than as `false`, because the receiver reads absent as
        // "on" — the same thing an absent value in the policy file means.
        for flag in [Some(true), Some(false), None] {
            let message = SetEgressAllowlist {
                allowed_hosts: vec!["api.anthropic.com".into()],
                proxy_child_processes: flag,
            };
            let decoded = SetEgressAllowlist::decode(message.encode_to_vec().as_slice())
                .expect("the message should round-trip");
            assert_eq!(decoded.allowed_hosts, message.allowed_hosts);
            assert_eq!(decoded.proxy_child_processes, flag);
        }
    }

    #[test]
    fn add_worktree_without_the_field_holds_the_code_back() {
        // ADR-F50: what an IDE sends, and what a peer that does not know the
        // field sends, must both leave the decision to the IDE.
        let plain = AddWorktree {
            path: "/code".into(),
            project_id: REMOTE_SERVER_PROJECT_ID,
            visible: true,
            ..Default::default()
        };
        let decoded = AddWorktree::decode(plain.encode_to_vec().as_slice())
            .expect("the message should round-trip");
        assert!(!decoded.code_runs_without_ide_decision);

        // The retired `code_execution_follows_ide = 10000` set to true, as a
        // revision 1 client wrote it.
        let mut revision_1 = plain.encode_to_vec();
        prost::encoding::bool::encode(10000, &true, &mut revision_1);
        let decoded =
            AddWorktree::decode(revision_1.as_slice()).expect("an unknown field is skipped");
        assert_eq!(decoded.path, "/code");
        assert!(
            !decoded.code_runs_without_ide_decision,
            "the old field must not read as the new one"
        );

        let opted_out = AddWorktree {
            code_runs_without_ide_decision: true,
            ..plain
        };
        let decoded = AddWorktree::decode(opted_out.encode_to_vec().as_slice())
            .expect("the message should round-trip");
        assert!(decoded.code_runs_without_ide_decision);
    }

    #[test]
    fn wire_revision_check_round_trips_as_a_request() {
        let request = CheckWireRevision {
            revision: JP_ENHANCE_WIRE_REVISION,
            display_language: Some("pt-BR".to_string()),
        };
        let envelope = request.clone().into_envelope(3, None, None);
        assert_eq!(CheckWireRevision::from_envelope(envelope), Some(request));
        let response = CheckWireRevisionResponse {
            revision: JP_ENHANCE_WIRE_REVISION,
        }
        .into_envelope(4, Some(3), None);
        assert_eq!(
            <CheckWireRevision as RequestMessage>::Response::from_envelope(response)
                .map(|response| response.revision),
            Some(JP_ENHANCE_WIRE_REVISION)
        );
        // A response that says nothing names no revision, which never matches.
        let empty = CheckWireRevisionResponse::decode([].as_slice()).expect("empty decodes");
        assert_ne!(empty.revision, JP_ENHANCE_WIRE_REVISION);
    }

    #[test]
    fn project_launch_approval_messages_fail_closed_and_round_trip() {
        // ADR-F50: a response that says nothing must not read as an approval.
        let empty = RequestProjectLaunchApprovalResponse::decode([].as_slice())
            .expect("an empty message decodes");
        assert_eq!(
            ProjectLaunchApprovalState::try_from(empty.state).ok(),
            Some(ProjectLaunchApprovalState::LaunchApprovalUnavailable)
        );
        assert_eq!(
            ProjectLaunchKind::try_from(RequestProjectLaunchApproval::default().kind).ok(),
            Some(ProjectLaunchKind::LaunchKindUnknown)
        );

        let request = RequestProjectLaunchApproval {
            project_id: REMOTE_SERVER_PROJECT_ID,
            remote_key: "0".repeat(64),
            kind: ProjectLaunchKind::LaunchKindLanguageServerBinary as i32,
            name: "project_lsp".into(),
            program: "node".into(),
            arguments: vec!["\u{202E}sj.exe".into()],
            definition_file: Some(
                request_project_launch_approval::DefinitionFile::ServerSettings(
                    request_project_launch_approval::ServerSettings {},
                ),
            ),
            ..Default::default()
        };
        let envelope = request.clone().into_envelope(7, None, None);
        let decoded =
            RequestProjectLaunchApproval::from_envelope(envelope).expect("the envelope carries it");
        assert_eq!(decoded, request);
        assert_eq!(decoded.remote_entity_id(), REMOTE_SERVER_PROJECT_ID);
    }

    #[test]
    fn proxy_http_response_frames_round_trip_and_reassemble() {
        // ADR-F27: a body larger than one chunk is carried by several
        // `ProxyHttpResponse` frames sharing a `request_id`; only the first
        // one carries the status line and the headers.
        const CHUNK: usize = 4;
        let body = b"0123456789ab".to_vec();
        let mut frames = Vec::new();
        for (index, chunk) in body.chunks(CHUNK).enumerate() {
            frames.push(ProxyHttpResponse {
                request_id: 7,
                status: if index == 0 { 200 } else { 0 },
                headers: if index == 0 {
                    vec![ProxyHttpHeader {
                        name: "content-type".into(),
                        value: "application/octet-stream".into(),
                    }]
                } else {
                    Vec::new()
                },
                chunk: chunk.to_vec(),
                is_last: false,
                error: None,
            });
        }
        frames.push(ProxyHttpResponse {
            request_id: 7,
            is_last: true,
            ..Default::default()
        });

        let decoded = frames
            .iter()
            .map(|frame| {
                ProxyHttpResponse::decode(frame.encode_to_vec().as_slice())
                    .expect("the frame should round-trip")
            })
            .collect::<Vec<_>>();

        assert!(decoded.iter().all(|frame| frame.request_id == 7));
        assert_eq!(decoded[0].status, 200);
        assert_eq!(decoded[0].headers[0].name, "content-type");
        assert_eq!(
            decoded.iter().filter(|frame| frame.is_last).count(),
            1,
            "exactly one frame ends the body"
        );
        let reassembled = decoded
            .iter()
            .flat_map(|frame| frame.chunk.iter().copied())
            .collect::<Vec<_>>();
        assert_eq!(reassembled, body);
    }

    #[test]
    fn proxy_http_error_round_trips_the_egress_denial() {
        // ADR-F27: the denial has to survive as its own variant, because the
        // remote host rebuilds `http_client::EgressDenied` from it so the
        // ADR-F22 consent flow can classify the failure.
        let message = ProxyHttpResponse {
            request_id: 1,
            is_last: true,
            error: Some(ProxyHttpError {
                kind: Some(proxy_http_error::Kind::EgressDenied(
                    ProxyHttpEgressDenied {
                        host: "github.com".into(),
                        url: "https://github.com/clangd/clangd/releases".into(),
                    },
                )),
            }),
            ..Default::default()
        };
        let decoded = ProxyHttpResponse::decode(message.encode_to_vec().as_slice())
            .expect("the message should round-trip");
        assert_eq!(decoded.error, message.error);
    }

    #[test]
    fn test_split_repository_update_keeps_branch_list_unchanged_on_final_chunk() {
        let update = UpdateRepository {
            updated_statuses: vec![StatusEntry::default(), StatusEntry::default()],
            branch_list: Vec::new(),
            branch_list_unchanged: true,
            ..Default::default()
        };

        let chunks = split_repository_update(update).collect::<Vec<_>>();

        assert_eq!(chunks.len(), 2);
        assert!(
            !chunks[0].branch_list_unchanged,
            "intermediate chunks never carry the branch list, so they must not claim it is unchanged"
        );
        assert!(chunks[1].branch_list_unchanged);
        assert!(chunks[1].is_last_update);
    }

    #[test]
    fn test_update_repository_branch_list_unchanged_round_trips() {
        for flag in [false, true] {
            let message = UpdateRepository {
                branch_list_unchanged: flag,
                ..Default::default()
            };
            let encoded = message.encode_to_vec();
            let decoded = UpdateRepository::decode(encoded.as_slice())
                .expect("the message should round-trip");
            assert_eq!(decoded.branch_list_unchanged, flag);
        }

        // A message from a peer that never set the field (an older build) must
        // decode as `false`, i.e. as a full replacement of the branch list.
        let without_flag = UpdateRepository::default().encode_to_vec();
        let decoded =
            UpdateRepository::decode(without_flag.as_slice()).expect("the message should decode");
        assert!(!decoded.branch_list_unchanged);
    }

    #[test]
    fn test_split_repository_update_keeps_status_error_on_final_chunk() {
        let update = UpdateRepository {
            updated_statuses: vec![StatusEntry::default(), StatusEntry::default()],
            status_error: Some("git status failed".into()),
            ..Default::default()
        };

        let chunks = split_repository_update(update).collect::<Vec<_>>();

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].status_error, None);
        assert_eq!(chunks[1].status_error.as_deref(), Some("git status failed"));
        assert!(chunks[1].is_last_update);

        // An older peer never sets the field, which must read as "no error".
        let without_error = UpdateRepository::default().encode_to_vec();
        let decoded =
            UpdateRepository::decode(without_error.as_slice()).expect("the message should decode");
        assert_eq!(decoded.status_error, None);
    }
}
