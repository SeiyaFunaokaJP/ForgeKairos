#![allow(non_snake_case)]

pub mod error;
mod macros;
mod typed_envelope;

pub use error::*;
pub use prost::{DecodeError, Message};
use std::{
    cmp,
    fmt::Debug,
    iter, mem,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
pub use typed_envelope::*;

include!(concat!(env!("OUT_DIR"), "/zed.messages.rs"));

pub const REMOTE_SERVER_PEER_ID: PeerId = PeerId { owner_id: 0, id: 0 };
pub const REMOTE_SERVER_PROJECT_ID: u64 = 0;

messages!(
    (Ack, Foreground),
    (AckBufferOperation, Background),
    (AckChannelMessage, Background),
    (ActivateToolchain, Foreground),
    (ActiveToolchain, Foreground),
    (ActiveToolchainResponse, Foreground),
    (ResolveToolchain, Background),
    (ResolveToolchainResponse, Background),
    (AddNotification, Foreground),
    (AddProjectCollaborator, Foreground),
    (AddWorktree, Foreground),
    (AddWorktreeResponse, Foreground),
    (AllocateWorktreeId, Foreground),
    (AllocateWorktreeIdResponse, Foreground),
    (ApplyCodeAction, Background),
    (ApplyCodeActionResponse, Background),
    (ApplyCompletionAdditionalEdits, Background),
    (ApplyCompletionAdditionalEditsResponse, Background),
    (BlameBuffer, Foreground),
    (BlameBufferResponse, Foreground),
    (BufferReloaded, Foreground),
    (BufferSaved, Foreground),
    (Call, Foreground),
    (CallCanceled, Foreground),
    (CancelCall, Foreground),
    (CancelLanguageServerWork, Foreground),
    (ChannelMessageSent, Foreground),
    (ChannelMessageUpdate, Foreground),
    (CloseBuffer, Foreground),
    (Commit, Background),
    (RunGitHook, Background),
    (CopyProjectEntry, Foreground),
    (CreateBufferForPeer, Foreground),
    (CreateImageForPeer, Foreground),
    (CreateFileForPeer, Foreground),
    (CreateChannel, Foreground),
    (CreateChannelResponse, Foreground),
    (CreateProjectEntry, Foreground),
    (CreateRoom, Foreground),
    (CreateRoomResponse, Foreground),
    (DeclineCall, Foreground),
    (DeleteChannel, Foreground),
    (DeleteNotification, Foreground),
    (DeleteProjectEntry, Foreground),
    (TrashProjectEntry, Foreground),
    (TrashProjectEntryResponse, Foreground),
    (RestoreProjectEntry, Foreground),
    (RestoreProjectEntryResponse, Foreground),
    (DownloadFileByPath, Background),
    (DownloadFileResponse, Background),
    (EndStream, Foreground),
    (Error, Foreground),
    (ExpandProjectEntry, Foreground),
    (ExpandProjectEntryResponse, Foreground),
    (FindSearchCandidates, Background),
    (FlushBufferedMessages, Foreground),
    (ExpandAllForProjectEntry, Foreground),
    (ExpandAllForProjectEntryResponse, Foreground),
    (RescanProjectWorktree, Foreground),
    (RescanProjectWorktreeResponse, Foreground),
    (RequestProjectLaunchApproval, Foreground),
    (RequestProjectLaunchApprovalResponse, Foreground),
    (ProjectLaunchApprovalDecided, Foreground),
    (ProjectLaunchApprovalRevoked, Foreground),
    (WithdrawProjectLaunchApproval, Foreground),
    (SetWorktreeCodeExecution, Foreground),
    (Follow, Foreground),
    (FollowResponse, Foreground),
    (ApplyCodeActionKind, Foreground),
    (ApplyCodeActionKindResponse, Foreground),
    (FormatBuffers, Foreground),
    (FormatBuffersResponse, Foreground),
    (FuzzySearchUsers, Foreground),
    (GetChannelMembers, Foreground),
    (GetChannelMembersResponse, Foreground),
    (GetChannelMessages, Background),
    (GetChannelMessagesById, Background),
    (GetChannelMessagesResponse, Background),
    (GetCodeActions, Background),
    (GetCodeActionsResponse, Background),
    (GetCompletions, Background),
    (GetCompletionsResponse, Background),
    (GetDeclaration, Background),
    (GetDeclarationResponse, Background),
    (GetDefinition, Background),
    (GetDefinitionResponse, Background),
    (GetEditPredictionDefinition, Background),
    (GetEditPredictionDefinitionResponse, Background),
    (GetEditPredictionTypeDefinition, Background),
    (GetEditPredictionTypeDefinitionResponse, Background),
    (GetDocumentHighlights, Background),
    (GetDocumentHighlightsResponse, Background),
    (GetDocumentSymbols, Background),
    (GetDocumentSymbolsResponse, Background),
    (GetHover, Background),
    (GetHoverResponse, Background),
    (GetNotifications, Foreground),
    (GetNotificationsResponse, Foreground),
    (GetCrashFiles, Background),
    (GetCrashFilesResponse, Background),
    (GetPathMetadata, Background),
    (GetPathMetadataResponse, Background),
    (GetPermalinkToLine, Foreground),
    (GetProcesses, Background),
    (GetProcessesResponse, Background),
    (GetPermalinkToLineResponse, Foreground),
    (GetProjectSymbols, Background),
    (GetProjectSymbolsResponse, Background),
    (GetReferences, Background),
    (GetReferencesResponse, Background),
    (GetSignatureHelp, Background),
    (GetSignatureHelpResponse, Background),
    (GetTypeDefinition, Background),
    (GetTypeDefinitionResponse, Background),
    (GetImplementation, Background),
    (GetImplementationResponse, Background),
    (OpenUnstagedDiff, Foreground),
    (OpenUnstagedDiffResponse, Foreground),
    (OpenUncommittedDiff, Foreground),
    (OpenUncommittedDiffResponse, Foreground),
    (GetUsers, Foreground),
    (GitGetBranches, Background),
    (GitBranchesResponse, Background),
    (Hello, Foreground),
    (HideToast, Background),
    (IncomingCall, Foreground),
    (InlayHints, Background),
    (InlayHintsResponse, Background),
    (SemanticTokens, Background),
    (SemanticTokensResponse, Background),
    (InstallExtension, Background),
    (InviteChannelMember, Foreground),
    (JoinChannel, Foreground),
    (JoinChannelBuffer, Foreground),
    (JoinChannelBufferResponse, Foreground),
    (JoinChannelChat, Foreground),
    (JoinChannelChatResponse, Foreground),
    (JoinProject, Foreground),
    (JoinProjectResponse, Foreground),
    (JoinRoom, Foreground),
    (JoinRoomResponse, Foreground),
    (LanguageServerLog, Foreground),
    (LanguageServerPromptRequest, Foreground),
    (LanguageServerPromptResponse, Foreground),
    (LeaveChannelBuffer, Background),
    (LeaveChannelChat, Foreground),
    (LeaveProject, Foreground),
    (LeaveRoom, Foreground),
    (LinkedEditingRange, Background),
    (LinkedEditingRangeResponse, Background),
    (ListRemoteDirectory, Background),
    (ListRemoteDirectoryResponse, Background),
    (ListToolchains, Foreground),
    (ListToolchainsResponse, Foreground),
    (LoadCommitDiff, Foreground),
    (LoadCommitDiffResponse, Foreground),
    (LspExtExpandMacro, Background),
    (LspExtExpandMacroResponse, Background),
    (LspExtOpenDocs, Background),
    (LspExtOpenDocsResponse, Background),
    (LspExtRunnables, Background),
    (LspExtRunnablesResponse, Background),
    (LspExtSwitchSourceHeader, Background),
    (LspExtSwitchSourceHeaderResponse, Background),
    (LspExtGoToParentModule, Background),
    (LspExtGoToParentModuleResponse, Background),
    (LspExtCancelFlycheck, Background),
    (LspExtRunFlycheck, Background),
    (LspExtClearFlycheck, Background),
    (MarkNotificationRead, Foreground),
    (MoveChannel, Foreground),
    (ReorderChannel, Foreground),
    (LspQuery, Background),
    (LspQueryResponse, Background),
    (OnTypeFormatting, Background),
    (OnTypeFormattingResponse, Background),
    (OpenBufferById, Background),
    (OpenBufferByPath, Background),
    (OpenImageByPath, Background),
    (OpenBufferForSymbol, Background),
    (OpenBufferForSymbolResponse, Background),
    (OpenBufferResponse, Background),
    (OpenImageResponse, Background),
    (OpenCommitMessageBuffer, Background),
    (OpenNewBuffer, Foreground),
    (OpenServerSettings, Foreground),
    (PerformRename, Background),
    (PerformRenameResponse, Background),
    (Ping, Foreground),
    (PrepareRename, Background),
    (PrepareRenameResponse, Background),
    (ProjectEntryResponse, Foreground),
    (RefreshInlayHints, Background),
    (RefreshSemanticTokens, Background),
    (RefreshDocumentColors, Background),
    (RefreshDocumentLinks, Background),
    (RefreshFoldingRanges, Background),
    (RefreshDocumentSymbols, Background),
    (RegisterBufferWithLanguageServers, Background),
    (RejoinChannelBuffers, Foreground),
    (RejoinChannelBuffersResponse, Foreground),
    (RejoinRemoteProjects, Foreground),
    (RejoinRemoteProjectsResponse, Foreground),
    (RejoinRoom, Foreground),
    (RejoinRoomResponse, Foreground),
    (ReloadBuffers, Foreground),
    (ReloadBuffersResponse, Foreground),
    (RemoveChannelMember, Foreground),
    (RemoveChannelMessage, Foreground),
    (RemoveContact, Foreground),
    (RemoveProjectCollaborator, Foreground),
    (RemoveWorktree, Foreground),
    (RenameChannel, Foreground),
    (RenameChannelResponse, Foreground),
    (RenameProjectEntry, Foreground),
    (RequestContact, Foreground),
    (ResolveCompletionDocumentation, Background),
    (ResolveCompletionDocumentationResponse, Background),
    (ResolveInlayHint, Background),
    (ResolveInlayHintResponse, Background),
    (GetDocumentColor, Background),
    (GetDocumentColorResponse, Background),
    (GetColorPresentation, Background),
    (GetColorPresentationResponse, Background),
    (GetDocumentLinks, Background),
    (GetDocumentLinksResponse, Background),
    (ResolveDocumentLink, Background),
    (ResolveDocumentLinkResponse, Background),
    (GetFoldingRanges, Background),
    (GetFoldingRangesResponse, Background),
    (RefreshCodeLens, Background),
    (GetCodeLens, Background),
    (GetCodeLensResponse, Background),
    (ResolveCodeAction, Background),
    (ResolveCodeActionResponse, Background),
    (RespondToChannelInvite, Foreground),
    (RespondToContactRequest, Foreground),
    (RestartLanguageServers, Foreground),
    (StopLanguageServers, Background),
    (RoomUpdated, Foreground),
    (SaveBuffer, Foreground),
    (SaveBufferVerified, Foreground),
    (VerifyAgentRead, Foreground),
    (ListSandboxProcesses, Background),
    (ListSandboxProcessesResponse, Background),
    (StopSandboxProcesses, Background),
    (StopSandboxProcessesResponse, Background),
    (SendChannelMessage, Background),
    (SendChannelMessageResponse, Background),
    (SetChannelMemberRole, Foreground),
    (SetChannelVisibility, Foreground),
    (SetEgressAllowlist, Background),
    (ProxyHttpRequest, Background),
    (ProxyHttpResponse, Background),
    (RunTransferCommand, Background),
    (RunTransferCommandResponse, Background),
    (CancelTransferCommand, Background),
    (TransferAskPassRequest, Background),
    (TransferAskPassResponse, Background),
    (ListTransferDirectory, Background),
    (ListTransferDirectoryResponse, Background),
    (TransferFileOperation, Background),
    (CheckDevTools, Background),
    (CheckDevToolsResponse, Background),
    (InstallDevTool, Background),
    (InstallDevToolResponse, Background),
    (SetRoomParticipantRole, Foreground),
    (ShareProject, Foreground),
    (ShareProjectResponse, Foreground),
    (ShowContacts, Foreground),
    (ShutdownRemoteServer, Foreground),
    (Stage, Background),
    (StartLanguageServer, Foreground),
    (SubscribeToChannels, Foreground),
    (SyncExtensions, Background),
    (SyncExtensionsResponse, Background),
    (BreakpointsForFile, Background),
    (ToggleBreakpoint, Foreground),
    (SynchronizeBuffers, Foreground),
    (SynchronizeBuffersResponse, Foreground),
    (TaskContext, Background),
    (TaskContextForLocation, Background),
    (TelemetryEvent, Background),
    (Test, Foreground),
    (Toast, Background),
    (Unfollow, Foreground),
    (UnshareProject, Foreground),
    (Unstage, Background),
    (Stash, Background),
    (StashPop, Background),
    (StashApply, Background),
    (StashDrop, Background),
    (UpdateBuffer, Foreground),
    (UpdateBufferFile, Foreground),
    (UpdateChannelBuffer, Foreground),
    (UpdateChannelBufferCollaborators, Foreground),
    (UpdateChannelMessage, Foreground),
    (UpdateChannels, Foreground),
    (UpdateContacts, Foreground),
    (UpdateDiagnosticSummary, Foreground),
    (UpdateDiffBases, Foreground),
    (UpdateFollowers, Foreground),
    (UpdateGitBranch, Background),
    (UpdateLanguageServer, Foreground),
    (UpdateNotification, Foreground),
    (UpdateParticipantLocation, Foreground),
    (UpdateProject, Foreground),
    (UpdateProjectCollaborator, Foreground),
    (UpdateUserChannels, Foreground),
    (UpdateWorktree, Foreground),
    (UpdateWorktreeSettings, Foreground),
    (UpdateUserSettings, Background),
    (UpdateRepository, Foreground),
    (RemoveRepository, Foreground),
    (UsersResponse, Foreground),
    (GitReset, Background),
    (GitDeleteBranch, Background),
    (GitCheckoutFiles, Background),
    (GitAddPathToGitignore, Background),
    (GitAddPathToGitInfoExclude, Background),
    (GitShow, Background),
    (GitCommitDetails, Background),
    (GitCreateCheckpoint, Background),
    (GitCreateCheckpointResponse, Background),
    (GitCreateArchiveCheckpoint, Background),
    (GitCreateArchiveCheckpointResponse, Background),
    (GitRestoreCheckpoint, Background),
    (GitRestoreArchiveCheckpoint, Background),
    (GitCompareCheckpoints, Background),
    (GitCompareCheckpointsResponse, Background),
    (GitDiffCheckpoints, Background),
    (GitDiffCheckpointsResponse, Background),
    (SetIndexText, Background),
    (Push, Background),
    (Fetch, Background),
    (GetRemotes, Background),
    (GetRemotesResponse, Background),
    (Pull, Background),
    (RemoteMessageResponse, Background),
    (AskPassRequest, Background),
    (AskPassResponse, Background),
    (GitCreateRemote, Background),
    (GitRemoveRemote, Background),
    (GitCreateBranch, Background),
    (GitChangeBranch, Background),
    (GitRenameBranch, Background),
    (TrustWorktrees, Background),
    (RestrictWorktrees, Background),
    (CheckForPushedCommits, Background),
    (CheckForPushedCommitsResponse, Background),
    (GitDiff, Background),
    (GitDiffResponse, Background),
    (GitRunCommand, Background),
    (GitRunCommandResponse, Background),
    (GitInit, Background),
    (GetDebugAdapterBinary, Background),
    (DebugAdapterBinary, Background),
    (RunDebugLocators, Background),
    (DebugRequest, Background),
    (LogToDebugConsole, Background),
    (GetDocumentDiagnostics, Background),
    (GetDocumentDiagnosticsResponse, Background),
    (PullWorkspaceDiagnostics, Background),
    (GetDefaultBranch, Background),
    (GetDefaultBranchResponse, Background),
    (GetTreeDiff, Background),
    (GetTreeDiffResponse, Background),
    (GetBlobContent, Background),
    (GetBlobContentResponse, Background),
    (BlameBufferAtRevision, Background),
    (BlameBufferAtRevisionResponse, Background),
    (LoadCommitTemplate, Background),
    (LoadCommitTemplateResponse, Background),
    (GitClone, Background),
    (GitCloneResponse, Background),
    (ToggleLspLogs, Background),
    (GetDirectoryEnvironment, Background),
    (DirectoryEnvironment, Background),
    (GetAgentServerCommand, Background),
    (AgentServerCommand, Background),
    (GetContextServerCommand, Background),
    (ContextServerCommand, Background),
    (ExternalAgentsUpdated, Background),
    (ExternalExtensionAgentsUpdated, Background),
    (ExternalAgentLoadingStatusUpdated, Background),
    (NewExternalAgentVersionAvailable, Background),
    (RemoteStarted, Background),
    (GitGetWorktrees, Background),
    (GitGetHeadSha, Background),
    (GitGetHeadShaResponse, Background),
    (GitEditRef, Background),
    (GitRepairWorktrees, Background),
    (GetCommitData, Background),
    (GetCommitDataResponse, Background),
    (GetInitialGraphData, Background),
    (GetInitialGraphDataResponse, Background),
    (SearchCommits, Background),
    (SearchCommitsResponse, Background),
    (GitWorktreesResponse, Background),
    (GitCreateWorktree, Background),
    (GitRemoveWorktree, Background),
    (GitRenameWorktree, Background),
    (GitWorktreeCreatedAt, Background),
    (GitWorktreeCreatedAtResponse, Background),
    (FindSearchCandidatesChunk, Background),
    (FindSearchCandidatesCancelled, Background),
    (SpawnKernel, Background),
    (SpawnKernelResponse, Background),
    (KillKernel, Background),
    (GetRemoteProfilingData, Background),
    (GetRemoteProfilingDataResponse, Background),
);

request_messages!(
    (AllocateWorktreeId, AllocateWorktreeIdResponse),
    (ApplyCodeAction, ApplyCodeActionResponse),
    (
        ApplyCompletionAdditionalEdits,
        ApplyCompletionAdditionalEditsResponse
    ),
    (Call, Ack),
    (CancelCall, Ack),
    (Commit, Ack),
    (RunGitHook, Ack),
    (CopyProjectEntry, ProjectEntryResponse),
    (CreateChannel, CreateChannelResponse),
    (CreateProjectEntry, ProjectEntryResponse),
    (CreateRoom, CreateRoomResponse),
    (DeclineCall, Ack),
    (DeleteChannel, Ack),
    (DeleteProjectEntry, ProjectEntryResponse),
    (TrashProjectEntry, TrashProjectEntryResponse),
    (RestoreProjectEntry, RestoreProjectEntryResponse),
    (DownloadFileByPath, DownloadFileResponse),
    (ExpandProjectEntry, ExpandProjectEntryResponse),
    (ExpandAllForProjectEntry, ExpandAllForProjectEntryResponse),
    (RescanProjectWorktree, RescanProjectWorktreeResponse),
    (SetWorktreeCodeExecution, Ack),
    (
        RequestProjectLaunchApproval,
        RequestProjectLaunchApprovalResponse
    ),
    (ProjectLaunchApprovalDecided, Ack),
    (ProjectLaunchApprovalRevoked, Ack),
    (WithdrawProjectLaunchApproval, Ack),
    (Follow, FollowResponse),
    (ApplyCodeActionKind, ApplyCodeActionKindResponse),
    (FormatBuffers, FormatBuffersResponse),
    (FuzzySearchUsers, UsersResponse),
    (GetChannelMembers, GetChannelMembersResponse),
    (GetChannelMessages, GetChannelMessagesResponse),
    (GetChannelMessagesById, GetChannelMessagesResponse),
    (GetCodeActions, GetCodeActionsResponse),
    (GetCompletions, GetCompletionsResponse),
    (GetDefinition, GetDefinitionResponse),
    (
        GetEditPredictionDefinition,
        GetEditPredictionDefinitionResponse
    ),
    (
        GetEditPredictionTypeDefinition,
        GetEditPredictionTypeDefinitionResponse
    ),
    (GetDeclaration, GetDeclarationResponse),
    (GetImplementation, GetImplementationResponse),
    (GetDocumentHighlights, GetDocumentHighlightsResponse),
    (GetDocumentSymbols, GetDocumentSymbolsResponse),
    (GetHover, GetHoverResponse),
    (GetNotifications, GetNotificationsResponse),
    (GetProjectSymbols, GetProjectSymbolsResponse),
    (GetReferences, GetReferencesResponse),
    (GetSignatureHelp, GetSignatureHelpResponse),
    (OpenUnstagedDiff, OpenUnstagedDiffResponse),
    (OpenUncommittedDiff, OpenUncommittedDiffResponse),
    (GetTypeDefinition, GetTypeDefinitionResponse),
    (LinkedEditingRange, LinkedEditingRangeResponse),
    (ListRemoteDirectory, ListRemoteDirectoryResponse),
    (GetUsers, UsersResponse),
    (IncomingCall, Ack),
    (InlayHints, InlayHintsResponse),
    (SemanticTokens, SemanticTokensResponse),
    (GetCodeLens, GetCodeLensResponse),
    (InviteChannelMember, Ack),
    (JoinChannel, JoinRoomResponse),
    (JoinChannelBuffer, JoinChannelBufferResponse),
    (JoinChannelChat, JoinChannelChatResponse),
    (JoinProject, JoinProjectResponse),
    (JoinRoom, JoinRoomResponse),
    (LeaveChannelBuffer, Ack),
    (LeaveRoom, Ack),
    (LoadCommitDiff, LoadCommitDiffResponse),
    (MarkNotificationRead, Ack),
    (MoveChannel, Ack),
    (OnTypeFormatting, OnTypeFormattingResponse),
    (OpenBufferById, OpenBufferResponse),
    (OpenBufferByPath, OpenBufferResponse),
    (OpenImageByPath, OpenImageResponse),
    (OpenBufferForSymbol, OpenBufferForSymbolResponse),
    (OpenCommitMessageBuffer, OpenBufferResponse),
    (OpenNewBuffer, OpenBufferResponse),
    (PerformRename, PerformRenameResponse),
    (Ping, Ack),
    (PrepareRename, PrepareRenameResponse),
    (RefreshInlayHints, Ack),
    (RefreshSemanticTokens, Ack),
    (RefreshCodeLens, Ack),
    (RefreshDocumentColors, Ack),
    (RefreshDocumentLinks, Ack),
    (RefreshFoldingRanges, Ack),
    (RefreshDocumentSymbols, Ack),
    (RejoinChannelBuffers, RejoinChannelBuffersResponse),
    (RejoinRoom, RejoinRoomResponse),
    (ReloadBuffers, ReloadBuffersResponse),
    (RemoveChannelMember, Ack),
    (RemoveChannelMessage, Ack),
    (UpdateChannelMessage, Ack),
    (RemoveContact, Ack),
    (RenameChannel, RenameChannelResponse),
    (RenameProjectEntry, ProjectEntryResponse),
    (ReorderChannel, Ack),
    (RequestContact, Ack),
    (
        ResolveCompletionDocumentation,
        ResolveCompletionDocumentationResponse
    ),
    (ResolveInlayHint, ResolveInlayHintResponse),
    (ResolveCodeAction, ResolveCodeActionResponse),
    (GetDocumentColor, GetDocumentColorResponse),
    (GetDocumentLinks, GetDocumentLinksResponse),
    (ResolveDocumentLink, ResolveDocumentLinkResponse),
    (GetFoldingRanges, GetFoldingRangesResponse),
    (GetColorPresentation, GetColorPresentationResponse),
    (RespondToChannelInvite, Ack),
    (RespondToContactRequest, Ack),
    (SaveBuffer, BufferSaved),
    (SaveBufferVerified, BufferSaved),
    (VerifyAgentRead, Ack),
    (ListSandboxProcesses, ListSandboxProcessesResponse),
    (StopSandboxProcesses, StopSandboxProcessesResponse),
    (Stage, Ack),
    (FindSearchCandidates, Ack),
    (SendChannelMessage, SendChannelMessageResponse),
    (SetChannelMemberRole, Ack),
    (SetChannelVisibility, Ack),
    (SetEgressAllowlist, Ack),
    (ProxyHttpRequest, Ack),
    (ProxyHttpResponse, Ack),
    (RunTransferCommand, RunTransferCommandResponse),
    (CancelTransferCommand, Ack),
    (TransferAskPassRequest, TransferAskPassResponse),
    (ListTransferDirectory, ListTransferDirectoryResponse),
    (TransferFileOperation, Ack),
    (CheckDevTools, CheckDevToolsResponse),
    (InstallDevTool, InstallDevToolResponse),
    (ShareProject, ShareProjectResponse),
    (SynchronizeBuffers, SynchronizeBuffersResponse),
    (TaskContextForLocation, TaskContext),
    (Test, Test),
    (Unstage, Ack),
    (Stash, Ack),
    (StashPop, Ack),
    (StashApply, Ack),
    (StashDrop, Ack),
    (UpdateBuffer, Ack),
    (UpdateParticipantLocation, Ack),
    (UpdateProject, Ack),
    (UpdateWorktree, Ack),
    (UpdateRepository, Ack),
    (RemoveRepository, Ack),
    (LspExtExpandMacro, LspExtExpandMacroResponse),
    (LspExtOpenDocs, LspExtOpenDocsResponse),
    (LspExtRunnables, LspExtRunnablesResponse),
    (SetRoomParticipantRole, Ack),
    (BlameBuffer, BlameBufferResponse),
    (RejoinRemoteProjects, RejoinRemoteProjectsResponse),
    (LspQuery, Ack),
    (LspQueryResponse, Ack),
    (RestartLanguageServers, Ack),
    (StopLanguageServers, Ack),
    (LspExtSwitchSourceHeader, LspExtSwitchSourceHeaderResponse),
    (LspExtGoToParentModule, LspExtGoToParentModuleResponse),
    (LspExtCancelFlycheck, Ack),
    (LspExtRunFlycheck, Ack),
    (LspExtClearFlycheck, Ack),
    (AddWorktree, AddWorktreeResponse),
    (ShutdownRemoteServer, Ack),
    (RemoveWorktree, Ack),
    (OpenServerSettings, OpenBufferResponse),
    (GetPermalinkToLine, GetPermalinkToLineResponse),
    (FlushBufferedMessages, Ack),
    (LanguageServerPromptRequest, LanguageServerPromptResponse),
    (GitGetBranches, GitBranchesResponse),
    (UpdateGitBranch, Ack),
    (ListToolchains, ListToolchainsResponse),
    (ActivateToolchain, Ack),
    (ActiveToolchain, ActiveToolchainResponse),
    (ResolveToolchain, ResolveToolchainResponse),
    (GetPathMetadata, GetPathMetadataResponse),
    (GetCrashFiles, GetCrashFilesResponse),
    (CancelLanguageServerWork, Ack),
    (SyncExtensions, SyncExtensionsResponse),
    (InstallExtension, Ack),
    (RegisterBufferWithLanguageServers, Ack),
    (GitShow, GitCommitDetails),
    (GitCreateCheckpoint, GitCreateCheckpointResponse),
    (
        GitCreateArchiveCheckpoint,
        GitCreateArchiveCheckpointResponse
    ),
    (GitRestoreCheckpoint, Ack),
    (GitRestoreArchiveCheckpoint, Ack),
    (GitCompareCheckpoints, GitCompareCheckpointsResponse),
    (GitDiffCheckpoints, GitDiffCheckpointsResponse),
    (GitReset, Ack),
    (GitDeleteBranch, Ack),
    (GitCheckoutFiles, Ack),
    (GitAddPathToGitignore, Ack),
    (GitAddPathToGitInfoExclude, Ack),
    (SetIndexText, Ack),
    (Push, RemoteMessageResponse),
    (Fetch, RemoteMessageResponse),
    (GetRemotes, GetRemotesResponse),
    (Pull, RemoteMessageResponse),
    (AskPassRequest, AskPassResponse),
    (GitCreateRemote, Ack),
    (GitRemoveRemote, Ack),
    (GitCreateBranch, Ack),
    (GitChangeBranch, Ack),
    (GitRenameBranch, Ack),
    (CheckForPushedCommits, CheckForPushedCommitsResponse),
    (GitDiff, GitDiffResponse),
    (GitRunCommand, GitRunCommandResponse),
    (GitInit, Ack),
    (ToggleBreakpoint, Ack),
    (GetDebugAdapterBinary, DebugAdapterBinary),
    (RunDebugLocators, DebugRequest),
    (GetDocumentDiagnostics, GetDocumentDiagnosticsResponse),
    (PullWorkspaceDiagnostics, Ack),
    (GetDefaultBranch, GetDefaultBranchResponse),
    (GetBlobContent, GetBlobContentResponse),
    (BlameBufferAtRevision, BlameBufferAtRevisionResponse),
    (LoadCommitTemplate, LoadCommitTemplateResponse),
    (GetTreeDiff, GetTreeDiffResponse),
    (GitClone, GitCloneResponse),
    (ToggleLspLogs, Ack),
    (GetDirectoryEnvironment, DirectoryEnvironment),
    (GetProcesses, GetProcessesResponse),
    (GetAgentServerCommand, AgentServerCommand),
    (GetContextServerCommand, ContextServerCommand),
    (RemoteStarted, Ack),
    (GitGetWorktrees, GitWorktreesResponse),
    (GitGetHeadSha, GitGetHeadShaResponse),
    (GitEditRef, Ack),
    (GitRepairWorktrees, Ack),
    (GetCommitData, GetCommitDataResponse),
    (GetInitialGraphData, GetInitialGraphDataResponse),
    (SearchCommits, SearchCommitsResponse),
    (GitCreateWorktree, Ack),
    (GitRemoveWorktree, Ack),
    (GitRenameWorktree, Ack),
    (GitWorktreeCreatedAt, GitWorktreeCreatedAtResponse),
    (TrustWorktrees, Ack),
    (RestrictWorktrees, Ack),
    (FindSearchCandidatesChunk, Ack),
    (SpawnKernel, SpawnKernelResponse),
    (KillKernel, Ack),
    (GetRemoteProfilingData, GetRemoteProfilingDataResponse),
);

lsp_messages!(
    (GetReferences, GetReferencesResponse, true),
    (GetDocumentColor, GetDocumentColorResponse, true),
    (GetFoldingRanges, GetFoldingRangesResponse, true),
    (GetDocumentSymbols, GetDocumentSymbolsResponse, true),
    (GetDocumentLinks, GetDocumentLinksResponse, true),
    (GetHover, GetHoverResponse, true),
    (GetCodeActions, GetCodeActionsResponse, true),
    (GetSignatureHelp, GetSignatureHelpResponse, true),
    (GetCodeLens, GetCodeLensResponse, true),
    (GetDocumentDiagnostics, GetDocumentDiagnosticsResponse, true),
    (GetDefinition, GetDefinitionResponse, true),
    (
        GetEditPredictionDefinition,
        GetEditPredictionDefinitionResponse,
        true
    ),
    (
        GetEditPredictionTypeDefinition,
        GetEditPredictionTypeDefinitionResponse,
        true
    ),
    (GetDeclaration, GetDeclarationResponse, true),
    (GetTypeDefinition, GetTypeDefinitionResponse, true),
    (GetImplementation, GetImplementationResponse, true),
    (InlayHints, InlayHintsResponse, false),
    (SemanticTokens, SemanticTokensResponse, true)
);

entity_messages!(
    {project_id, ShareProject},
    AddProjectCollaborator,
    AddWorktree,
    AllocateWorktreeId,
    ApplyCodeAction,
    ApplyCompletionAdditionalEdits,
    BlameBuffer,
    BufferReloaded,
    BufferSaved,
    CloseBuffer,
    Commit,
    RunGitHook,
    GetColorPresentation,
    CopyProjectEntry,
    CreateBufferForPeer,
    CreateFileForPeer,
    CreateImageForPeer,
    CreateProjectEntry,
    GetDocumentColor,
    GetDocumentLinks,
    ResolveDocumentLink,
    GetFoldingRanges,
    DeleteProjectEntry,
    TrashProjectEntry,
    RestoreProjectEntry,
    ExpandProjectEntry,
    ExpandAllForProjectEntry,
    RescanProjectWorktree,
    RequestProjectLaunchApproval,
    ProjectLaunchApprovalDecided,
    ProjectLaunchApprovalRevoked,
    WithdrawProjectLaunchApproval,
    SetWorktreeCodeExecution,
    FindSearchCandidates,
    ApplyCodeActionKind,
    FormatBuffers,
    GetCodeActions,
    GetCodeLens,
    GetCompletions,
    GetDefinition,
    GetEditPredictionDefinition,
    GetEditPredictionTypeDefinition,
    GetDeclaration,
    GetImplementation,
    GetDocumentHighlights,
    GetDocumentSymbols,
    GetHover,
    GetProjectSymbols,
    GetReferences,
    GetSignatureHelp,
    OpenUnstagedDiff,
    OpenUncommittedDiff,
    GetTypeDefinition,
    InlayHints,
    SemanticTokens,
    JoinProject,
    LeaveProject,
    SpawnKernel,
    KillKernel,
    LinkedEditingRange,
    LoadCommitDiff,
    LspQuery,
    LspQueryResponse,
    RestartLanguageServers,
    StopLanguageServers,
    OnTypeFormatting,
    OpenNewBuffer,
    OpenBufferById,
    OpenBufferByPath,
    OpenImageByPath,
    OpenBufferForSymbol,
    OpenCommitMessageBuffer,
    PerformRename,
    PrepareRename,
    RefreshInlayHints,
    RefreshSemanticTokens,
    RefreshCodeLens,
    RefreshDocumentColors,
    RefreshDocumentLinks,
    RefreshFoldingRanges,
    RefreshDocumentSymbols,
    ReloadBuffers,
    RemoveProjectCollaborator,
    RenameProjectEntry,
    ResolveCompletionDocumentation,
    ResolveInlayHint,
    ResolveCodeAction,
    SaveBuffer,
    SaveBufferVerified,
    VerifyAgentRead,
    ListSandboxProcesses,
    StopSandboxProcesses,
    Stage,
    StartLanguageServer,
    SynchronizeBuffers,
    TaskContextForLocation,
    UnshareProject,
    Unstage,
    Stash,
    StashPop,
    StashApply,
    StashDrop,
    UpdateBuffer,
    UpdateBufferFile,
    UpdateDiagnosticSummary,
    UpdateDiffBases,
    UpdateLanguageServer,
    UpdateProject,
    UpdateProjectCollaborator,
    UpdateWorktree,
    UpdateRepository,
    RemoveRepository,
    UpdateWorktreeSettings,
    UpdateUserSettings,
    LspExtExpandMacro,
    LspExtOpenDocs,
    LspExtRunnables,
    LspExtSwitchSourceHeader,
    LspExtGoToParentModule,
    LspExtCancelFlycheck,
    LspExtRunFlycheck,
    LspExtClearFlycheck,
    LanguageServerLog,
    TelemetryEvent,
    Toast,
    HideToast,
    OpenServerSettings,
    GetPermalinkToLine,
    LanguageServerPromptRequest,
    GitGetBranches,
    UpdateGitBranch,
    ListToolchains,
    ActivateToolchain,
    ActiveToolchain,
    ResolveToolchain,
    GetPathMetadata,
    GetProcesses,
    CancelLanguageServerWork,
    RegisterBufferWithLanguageServers,
    GitShow,
    GitCreateCheckpoint,
    GitRestoreCheckpoint,
    GitCompareCheckpoints,
    GitDiffCheckpoints,
    GitReset,
    GitDeleteBranch,
    GitCheckoutFiles,
    GitAddPathToGitignore,
    GitAddPathToGitInfoExclude,
    SetIndexText,
    ToggleLspLogs,
    GetDirectoryEnvironment,

    Push,
    Fetch,
    GetRemotes,
    Pull,
    AskPassRequest,
    RunTransferCommand,
    CancelTransferCommand,
    TransferAskPassRequest,
    ListTransferDirectory,
    TransferFileOperation,
    CheckDevTools,
    InstallDevTool,
    GitChangeBranch,
    GitRenameBranch,
    GitCreateBranch,
    GitCreateRemote,
    GitRemoveRemote,
    CheckForPushedCommits,
    GitDiff,
    GitRunCommand,
    GitInit,
    BreakpointsForFile,
    ToggleBreakpoint,
    RunDebugLocators,
    GetDebugAdapterBinary,
    LogToDebugConsole,
    GetDocumentDiagnostics,
    PullWorkspaceDiagnostics,
    GetDefaultBranch,
    GetTreeDiff,
    GetBlobContent,
    BlameBufferAtRevision,
    LoadCommitTemplate,
    GitClone,
    GetAgentServerCommand,
    GetContextServerCommand,
    ExternalAgentsUpdated,
    ExternalExtensionAgentsUpdated,
    ExternalAgentLoadingStatusUpdated,
    NewExternalAgentVersionAvailable,
    GitGetWorktrees,
    GitGetHeadSha,
    GitEditRef,
    GitRepairWorktrees,
    GetCommitData,
    GetInitialGraphData,
    SearchCommits,
    GitCreateArchiveCheckpoint,
    GitRestoreArchiveCheckpoint,
    GitCreateWorktree,
    GitRemoveWorktree,
    GitRenameWorktree,
    GitWorktreeCreatedAt,
    TrustWorktrees,
    RestrictWorktrees,
    FindSearchCandidatesChunk,
    FindSearchCandidatesCancelled,
    DownloadFileByPath,
    GetRemoteProfilingData
);

entity_messages!(
    {channel_id, Channel},
    ChannelMessageSent,
    ChannelMessageUpdate,
    RemoveChannelMessage,
    UpdateChannelMessage,
    UpdateChannelBuffer,
    UpdateChannelBufferCollaborators,
);

impl From<Timestamp> for SystemTime {
    fn from(val: Timestamp) -> Self {
        UNIX_EPOCH
            .checked_add(Duration::new(val.seconds, val.nanos))
            .unwrap()
    }
}

impl From<SystemTime> for Timestamp {
    fn from(time: SystemTime) -> Self {
        let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
        Self {
            seconds: duration.as_secs(),
            nanos: duration.subsec_nanos(),
        }
    }
}

impl From<u128> for Nonce {
    fn from(nonce: u128) -> Self {
        let upper_half = (nonce >> 64) as u64;
        let lower_half = nonce as u64;
        Self {
            upper_half,
            lower_half,
        }
    }
}

impl From<Nonce> for u128 {
    fn from(nonce: Nonce) -> Self {
        let upper_half = (nonce.upper_half as u128) << 64;
        let lower_half = nonce.lower_half as u128;
        upper_half | lower_half
    }
}

#[cfg(any(test, feature = "test-support"))]
pub const MAX_WORKTREE_UPDATE_MAX_CHUNK_SIZE: usize = 2;
#[cfg(not(any(test, feature = "test-support")))]
pub const MAX_WORKTREE_UPDATE_MAX_CHUNK_SIZE: usize = 256;

pub fn split_worktree_update(mut message: UpdateWorktree) -> impl Iterator<Item = UpdateWorktree> {
    let mut done = false;

    iter::from_fn(move || {
        if done {
            return None;
        }

        let updated_entries_chunk_size = cmp::min(
            message.updated_entries.len(),
            MAX_WORKTREE_UPDATE_MAX_CHUNK_SIZE,
        );
        let updated_entries: Vec<_> = message
            .updated_entries
            .drain(..updated_entries_chunk_size)
            .collect();

        let removed_entries_chunk_size = cmp::min(
            message.removed_entries.len(),
            MAX_WORKTREE_UPDATE_MAX_CHUNK_SIZE,
        );
        let removed_entries = message
            .removed_entries
            .drain(..removed_entries_chunk_size)
            .collect();

        let mut updated_repositories = Vec::new();
        let mut limit = MAX_WORKTREE_UPDATE_MAX_CHUNK_SIZE;
        while let Some(repo) = message.updated_repositories.first_mut() {
            let updated_statuses_limit = cmp::min(repo.updated_statuses.len(), limit);
            let removed_statuses_limit = cmp::min(repo.removed_statuses.len(), limit);

            updated_repositories.push(RepositoryEntry {
                repository_id: repo.repository_id,
                branch_summary: repo.branch_summary.clone(),
                updated_statuses: repo
                    .updated_statuses
                    .drain(..updated_statuses_limit)
                    .collect(),
                removed_statuses: repo
                    .removed_statuses
                    .drain(..removed_statuses_limit)
                    .collect(),
                current_merge_conflicts: repo.current_merge_conflicts.clone(),
            });
            if repo.removed_statuses.is_empty() && repo.updated_statuses.is_empty() {
                message.updated_repositories.remove(0);
            }
            limit = limit.saturating_sub(removed_statuses_limit + updated_statuses_limit);
            if limit == 0 {
                break;
            }
        }

        done = message.updated_entries.is_empty()
            && message.removed_entries.is_empty()
            && message.updated_repositories.is_empty();

        let removed_repositories = if done {
            mem::take(&mut message.removed_repositories)
        } else {
            Default::default()
        };

        Some(UpdateWorktree {
            project_id: message.project_id,
            worktree_id: message.worktree_id,
            root_name: message.root_name.clone(),
            abs_path: message.abs_path.clone(),
            root_repo_common_dir: message.root_repo_common_dir.clone(),
            root_repo_is_linked_worktree: message.root_repo_is_linked_worktree,
            updated_entries,
            removed_entries,
            scan_id: message.scan_id,
            is_last_update: done && message.is_last_update,
            updated_repositories,
            removed_repositories,
        })
    })
}

pub fn split_repository_update(
    mut update: UpdateRepository,
) -> impl Iterator<Item = UpdateRepository> {
    let mut updated_statuses_iter = mem::take(&mut update.updated_statuses).into_iter().fuse();
    let mut removed_statuses_iter = mem::take(&mut update.removed_statuses).into_iter().fuse();
    let branch_list = mem::take(&mut update.branch_list);
    let branch_list_error = update.branch_list_error.take();
    let branch_list_unchanged = mem::take(&mut update.branch_list_unchanged);
    let status_error = update.status_error.take();
    std::iter::from_fn({
        let update = update.clone();
        move || {
            let updated_statuses = updated_statuses_iter
                .by_ref()
                .take(MAX_WORKTREE_UPDATE_MAX_CHUNK_SIZE)
                .collect::<Vec<_>>();
            let removed_statuses = removed_statuses_iter
                .by_ref()
                .take(MAX_WORKTREE_UPDATE_MAX_CHUNK_SIZE)
                .collect::<Vec<_>>();
            if updated_statuses.is_empty() && removed_statuses.is_empty() {
                return None;
            }
            Some(UpdateRepository {
                updated_statuses,
                removed_statuses,
                branch_list: Vec::new(),
                branch_list_error: None,
                branch_list_unchanged: false,
                status_error: None,
                is_last_update: false,
                ..update.clone()
            })
        }
    })
    .chain([UpdateRepository {
        updated_statuses: Vec::new(),
        removed_statuses: Vec::new(),
        branch_list,
        branch_list_error,
        branch_list_unchanged,
        status_error,
        is_last_update: true,
        ..update
    }])
}

impl LspQuery {
    pub fn query_name_and_write_permissions(&self) -> (&str, bool) {
        match self.request {
            Some(lsp_query::Request::GetHover(_)) => ("GetHover", false),
            Some(lsp_query::Request::GetCodeActions(_)) => ("GetCodeActions", true),
            Some(lsp_query::Request::GetSignatureHelp(_)) => ("GetSignatureHelp", false),
            Some(lsp_query::Request::GetCodeLens(_)) => ("GetCodeLens", true),
            Some(lsp_query::Request::GetDocumentDiagnostics(_)) => {
                ("GetDocumentDiagnostics", false)
            }
            Some(lsp_query::Request::GetDefinition(_)) => ("GetDefinition", false),
            Some(lsp_query::Request::GetEditPredictionDefinition(_)) => {
                ("GetEditPredictionDefinition", false)
            }
            Some(lsp_query::Request::GetEditPredictionTypeDefinition(_)) => {
                ("GetEditPredictionTypeDefinition", false)
            }
            Some(lsp_query::Request::GetDeclaration(_)) => ("GetDeclaration", false),
            Some(lsp_query::Request::GetTypeDefinition(_)) => ("GetTypeDefinition", false),
            Some(lsp_query::Request::GetImplementation(_)) => ("GetImplementation", false),
            Some(lsp_query::Request::GetReferences(_)) => ("GetReferences", false),
            Some(lsp_query::Request::GetDocumentColor(_)) => ("GetDocumentColor", false),
            Some(lsp_query::Request::GetFoldingRanges(_)) => ("GetFoldingRanges", false),
            Some(lsp_query::Request::GetDocumentSymbols(_)) => ("GetDocumentSymbols", false),
            Some(lsp_query::Request::GetDocumentLinks(_)) => ("GetDocumentLinks", false),
            Some(lsp_query::Request::InlayHints(_)) => ("InlayHints", false),
            Some(lsp_query::Request::SemanticTokens(_)) => ("SemanticTokens", false),
            None => ("<unknown>", true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_converting_peer_id_from_and_to_u64() {
        let peer_id = PeerId {
            owner_id: 10,
            id: 3,
        };
        assert_eq!(PeerId::from_u64(peer_id.as_u64()), peer_id);
        let peer_id = PeerId {
            owner_id: u32::MAX,
            id: 3,
        };
        assert_eq!(PeerId::from_u64(peer_id.as_u64()), peer_id);
        let peer_id = PeerId {
            owner_id: 10,
            id: u32::MAX,
        };
        assert_eq!(PeerId::from_u64(peer_id.as_u64()), peer_id);
        let peer_id = PeerId {
            owner_id: u32::MAX,
            id: u32::MAX,
        };
        assert_eq!(PeerId::from_u64(peer_id.as_u64()), peer_id);
    }

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
    fn project_launch_approval_messages_fail_closed_and_round_trip() {
        // ADR-F50: a response that says nothing must not read as an approval.
        let empty = RequestProjectLaunchApprovalResponse::decode([].as_slice())
            .expect("an empty message decodes");
        assert_eq!(
            ProjectLaunchApprovalState::from_i32(empty.state),
            Some(ProjectLaunchApprovalState::LaunchApprovalUnavailable)
        );
        assert_eq!(
            ProjectLaunchKind::from_i32(RequestProjectLaunchApproval::default().kind),
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
    fn test_split_repository_update_keeps_branch_list_on_final_chunk() {
        let update = UpdateRepository {
            updated_statuses: vec![
                StatusEntry::default(),
                StatusEntry::default(),
                StatusEntry::default(),
            ],
            branch_list: vec![Branch {
                ref_name: "refs/heads/main".into(),
                ..Default::default()
            }],
            branch_list_error: Some("partial branch scan".into()),
            ..Default::default()
        };

        let chunks = split_repository_update(update).collect::<Vec<_>>();

        assert_eq!(chunks.len(), 3);
        assert!(chunks[0].branch_list.is_empty());
        assert!(chunks[1].branch_list.is_empty());
        assert_eq!(chunks[2].branch_list.len(), 1);
        assert_eq!(chunks[2].branch_list[0].ref_name, "refs/heads/main");
        assert_eq!(chunks[0].branch_list_error, None);
        assert_eq!(chunks[1].branch_list_error, None);
        assert_eq!(
            chunks[2].branch_list_error.as_deref(),
            Some("partial branch scan")
        );
        assert!(!chunks[0].is_last_update);
        assert!(!chunks[1].is_last_update);
        assert!(chunks[2].is_last_update);
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
