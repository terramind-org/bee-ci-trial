/**
 * The UI method table: every operation a screen performs, with its view-model request and
 * response. Names follow the screens, not the wire: `lib/api/ops/` maps each one to the real
 * bee.v1 RPCs (often the same name, sometimes several calls). The typed client derives
 * from this table, and `ops/index.ts` must implement every entry.
 */
import type { CreateOrgInvitationRequestJson, MemberJson, OrgInvitationJson, SetMemberRequestJson } from "../gen/bee/v1/forge_pb";
import type { ListTeamMembersRequestJson, ListTeamMembersResponseJson } from "../gen/bee/v1/teams_pb";
import type * as T from "./types";
import type { VerifyAuditChainRequestJson, VerifyAuditChainResponseJson } from "../gen/bee/v1/audit_pb";

export interface Methods {
  OrgService: {
    GetOrg: [T.GetOrgRequest, T.Org];
    ListOrgs: [T.ListOrgsRequest, T.ListOrgsResponse];
    ListMembers: [T.ListMembersRequest, T.ListMembersResponse];
    CreateOrg: [T.CreateOrgRequest, T.Org];
    // The organisation settings use these existing RPCs without reshaping their messages.
    SetMember: [SetMemberRequestJson, MemberJson];
    CreateOrgInvitation: [CreateOrgInvitationRequestJson, OrgInvitationJson];
  };
  RepoService: {
    GetRepo: [T.GetRepoRequest, T.Repo];
    UpdateRepo: [T.UpdateRepoRequest, T.Repo];
    ListCollaborators: [T.ListCollaboratorsRequest, T.ListCollaboratorsResponse];
    ListDeployKeys: [T.ListDeployKeysRequest, T.ListDeployKeysResponse];
    ListRepos: [T.ListReposRequest, T.ListReposResponse];
    GetContext: [T.GetContextRequest, T.RepoContext];
    ListMyRepos: [T.ListMyReposRequest, T.ListMyReposResponse];
    GetRepoHome: [T.GetRepoHomeRequest, T.RepoHome];
    CreateRepo: [T.CreateRepoRequest, T.Repo];
    ForkRepo: [T.ForkRepoRequest, T.Repo];
    ListForks: [T.ListForksRequest, T.ListForksResponse];
    SyncFork: [T.SyncForkRequest, T.SyncForkResponse];
  };
  ChangeService: {
    OpenChange: [T.OpenChangeRequest, T.ChangeRequest];
    GetChange: [T.GetChangeRequest, T.ChangeRequest];
    GetChangeByNumber: [T.GetChangeByNumberRequest, T.ChangeRequest];
    ListChanges: [T.ListChangesRequest, T.ListChangesResponse];
    CloseChange: [T.CloseChangeRequest, T.ChangeRequest];
    SubmitReview: [T.SubmitReviewRequest, T.Review];
    ListReviews: [T.ListReviewsRequest, T.ListReviewsResponse];
    EvaluateLanding: [T.EvaluateLandingRequest, T.LandingEvaluation];
    GetChangeDiff: [T.GetChangeDiffRequest, T.GetChangeDiffResponse];
    ListThreads: [T.ListThreadsRequest, T.ListThreadsResponse];
    CreateThread: [T.CreateThreadRequest, T.ThreadComment];
    ReplyThread: [T.ReplyThreadRequest, T.ThreadComment];
    ResolveThread: [T.ResolveThreadRequest, T.ReviewThread];
    ApplySuggestions: [T.ApplySuggestionsRequest, T.ChangeRequest];
    ListQueue: [T.ListQueueRequest, T.ListQueueResponse];
    Enqueue: [T.EnqueueRequest, T.QueueEntry];
    ListComments: [T.ListCommentsRequest, T.ListCommentsResponse];
    CommentChange: [T.CommentChangeRequest, T.Comment];
  };
  BriefService: {
    SetChangeBrief: [T.SetChangeBriefRequest, T.SetChangeBriefResponse];
    GetBriefChecklist: [T.GetBriefChecklistRequest, T.BriefChecklist];
    AddEvidence: [T.AddEvidenceRequest, T.Evidence];
    BeginEvidenceUpload: [T.BeginEvidenceUploadRequest, T.BeginEvidenceUploadResponse];
    FinalizeEvidence: [T.FinalizeEvidenceRequest, T.Evidence];
    GetEvidence: [T.GetEvidenceRequest, T.Evidence];
    ListEvidence: [T.ListEvidenceRequest, T.ListEvidenceResponse];
    UpdateEvidence: [T.UpdateEvidenceRequest, T.Evidence];
    DeleteEvidence: [T.DeleteEvidenceRequest, T.DeleteEvidenceResponse];
  };
  CheckService: {
    ListChecks: [T.ListChecksRequest, T.ListChecksResponse];
  };
  LeaseService: {
    AcquireLease: [T.AcquireLeaseRequest, T.AcquireLeaseResponse];
    ReleaseLease: [T.ReleaseLeaseRequest, T.ReleaseLeaseResponse];
    ListLeases: [T.ListLeasesRequest, T.ListLeasesResponse];
  };
  EventService: {
    Subscribe: [T.SubscribeRequest, T.Event];
    ListEvents: [T.ListEventsRequest, T.ListEventsResponse];
    GetEventLogInfo: [T.GetEventLogInfoRequest, T.EventLogInfo];
  };
  ObjectService: {
    ResolveRef: [T.ResolveRefRequest, T.ResolveRefResponse];
    GetServerInfo: [T.ServerInfoRequest, T.ServerInfo];
  };
  QueryService: {
    GetChangesets: [T.GetChangesetsRequest, T.GetChangesetsResponse];
    ReadPath: [T.ReadPathRequest, T.ReadPathChunk];
    ListTree: [T.ListTreeRequest, T.ListTreeResponse];
    Log: [T.LogRequest, T.LogResponse];
    IsAncestor: [T.IsAncestorRequest, T.IsAncestorResponse];
    Blame: [T.BlameRequest, T.BlameResponse];
    Diff: [T.DiffRequest, T.DiffItem];
    CompareRevisions: [T.CompareRevisionsRequest, T.CompareRevisionsResponse];
  };
  WriteService: {
    Land: [T.LandRequest, T.LandResponse];
  };
  RefService: {
    ListRefs: [T.ListRefsRequest, T.ListRefsResponse];
    GetRefLog: [T.GetRefLogRequest, T.GetRefLogResponse];
  };
  AuthService: {
    Login: [T.LoginRequest, T.LoginResponse];
    WhoAmI: [T.WhoAmIRequest, T.WhoAmIResponse];
    MintToken: [T.MintTokenRequest, T.MintTokenResponse];
    Revoke: [T.RevokeRequest, T.RevokeResponse];
  };
  ImportService: {
    StartImport: [T.StartImportRequest, T.Import];
    GetImport: [T.GetImportRequest, T.Import];
    ListImports: [T.ListImportsRequest, T.ListImportsResponse];
    SetImportPhase: [T.SetImportPhaseRequest, T.Import];
    CancelImport: [T.CancelImportRequest, T.Import];
    GetFidelityReport: [T.GetFidelityReportRequest, T.FidelityReport];
  };
  AgentService: {
    ResolveAgentInstructions: [T.ResolveAgentInstructionsRequest, T.ResolveAgentInstructionsResponse];
    ListAgents: [T.ListAgentsRequest, T.ListAgentsResponse];
    GetAgentBudget: [T.GetAgentBudgetRequest, T.AgentOwnerBudget];
    SetAgentBudget: [T.SetAgentBudgetRequest, T.AgentOwnerBudget];
    GetAgent: [T.GetAgentRequest, T.AgentSummary];
    ListTokens: [T.ListTokensRequest, T.ListTokensResponse];
    ListProvenance: [T.ListProvenanceRequest, T.ListProvenanceResponse];
    GetAgentFeedTrigger: [T.GetAgentFeedTriggerRequest, T.AgentFeedTrigger];
    SetAgentFeedTrigger: [T.SetAgentFeedTriggerRequest, T.AgentFeedTrigger];
    ListAgentFeedTriggers: [T.ListAgentFeedTriggersRequest, T.ListAgentFeedTriggersResponse];
    ListAdoptRuns: [T.ListAdoptRunsRequest, T.ListAdoptRunsResponse];
    GetAgentRun: [T.GetAgentRunRequest, T.AgentRun];
    ListAgentRuns: [T.ListAgentRunsRequest, T.ListAgentRunsResponse];
    ListAgentRunEvents: [T.ListAgentRunEventsRequest, T.ListAgentRunEventsResponse];
    SendAgentRunMessage: [T.SendAgentRunMessageRequest, T.AgentRunEvent];
    CancelAgentRun: [T.CancelAgentRunRequest, T.AgentRun];
    RetryAgentRun: [T.RetryAgentRunRequest, T.AgentRun];
    GetSessionsForChange: [T.GetSessionsForChangeRequest, T.GetSessionsForChangeResponse];
  };
  WorkflowService: {
    ListRuns: [T.ListRunsRequest, T.ListRunsResponse];
    GetRun: [T.GetRunRequest, T.Run];
    Rerun: [T.RerunRequest, T.Run];
    // CI management maps directly to the workflow and deployment services.
    CancelRun: [T.GetRunRequest, Record<string, never>];
    ListWorkflows: [T.ListWorkflowsRequest, T.ListWorkflowsResponse];
    EnableWorkflow: [T.WorkflowRequest, T.WorkflowDefinition];
    DisableWorkflow: [T.WorkflowRequest, T.WorkflowDefinition];
    GetWorkflowBadge: [T.WorkflowRequest, T.WorkflowBadge];
    StreamLogs: [T.StreamLogsRequest, T.LogLine];
  };
  DeploymentService: {
    ListDeployments: [T.ListDeploymentsRequest, T.ListDeploymentsResponse];
  };
  IssueService: {
    ListIssues: [T.ListIssuesRequest, T.ListIssuesResponse];
    GetIssue: [T.GetIssueRequest, T.Issue];
    CreateIssue: [T.CreateIssueRequest, T.Issue];
    AssignIssue: [T.AssignIssueRequest, T.Issue];
    SetIssueState: [T.SetIssueStateRequest, T.Issue];
    CommentIssue: [T.CommentIssueRequest, T.Issue];
  };
  ProjectService: {
    GetProject: [T.GetProjectRequest, T.Project];
    MoveItem: [T.MoveItemRequest, T.Project];
  };
  ReleaseService: {
    ListReleases: [T.ListReleasesRequest, T.ListReleasesResponse];
    GenerateNotes: [T.GenerateNotesRequest, T.GenerateNotesResponse];
    CreateRelease: [T.CreateReleaseRequest, T.Release];
    GetSourceArchiveUrl: [T.GetSourceArchiveUrlRequest, T.GetSourceArchiveUrlResponse];
  };
  SecurityService: {
    ListAlerts: [T.ListAlertsRequest, T.ListAlertsResponse];
    DismissAlert: [T.DismissAlertRequest, T.SecurityAlert];
  };
  PolicyService: {
    GetPolicy: [T.GetPolicyRequest, T.Policy];
    EvaluatePolicy: [T.EvaluatePolicyRequest, T.PolicyEvaluation];
  };
  AuditService: {
    ListAudit: [T.ListAuditRequest, T.ListAuditResponse];
    // Record exposes the existing verifier independently of the filtered query.
    VerifyAuditChain: [VerifyAuditChainRequestJson, VerifyAuditChainResponseJson];
  };
  PackageService: {
    ListPackages: [T.ListPackagesRequest, T.ListPackagesResponse];
  };
  PagesService: {
    GetPages: [T.GetPagesRequest, T.PagesView];
  };
  SearchService: {
    SearchCode: [T.SearchCodeRequest, T.SearchCodeResponse];
    SearchRepos: [T.SearchReposRequest, T.SearchReposResponse];
    ListTrendingRepos: [T.ListTrendingReposRequest, T.ListTrendingReposResponse];
    ListTrendingDevelopers: [T.ListTrendingDevelopersRequest, T.ListTrendingDevelopersResponse];
    ListLeaderboard: [T.ListLeaderboardRequest, T.ListLeaderboardResponse];
    GetTopic: [T.GetTopicRequest, T.Topic];
    ListFeaturedTopics: [T.ListFeaturedTopicsRequest, T.ListFeaturedTopicsResponse];
    ListPublicActivity: [T.ListPublicActivityRequest, T.ListPublicActivityResponse];
  };
  UserService: {
    GetProfile: [T.GetProfileRequest, T.Profile];
    GetViewer: [Record<string, never>, Pick<T.Profile, "login">];
    StarRepo: [T.StarRepoRequest, T.StarRepoResponse];
    UnstarRepo: [T.UnstarRepoRequest, T.UnstarRepoResponse];
    ListStarred: [T.ListStarredRequest, T.ListStarredResponse];
    FollowUser: [T.FollowUserRequest, T.FollowUserResponse];
    UnfollowUser: [T.UnfollowUserRequest, T.UnfollowUserResponse];
    ListFollowers: [T.ListFollowersRequest, T.ListFollowersResponse];
    ListFollowing: [T.ListFollowingRequest, T.ListFollowingResponse];
    FollowRepo: [T.FollowRepoRequest, T.FollowRepoResponse];
    UnfollowRepo: [T.UnfollowRepoRequest, T.UnfollowRepoResponse];
    IsFollowingRepo: [T.IsFollowingRepoRequest, T.IsFollowingRepoResponse];
    ListFollowedRepos: [T.ListFollowedReposRequest, T.ListFollowedReposResponse];
    ListRepoFollowers: [T.ListRepoFollowersRequest, T.ListRepoFollowersResponse];
    GetContributionGraph: [T.GetContributionGraphRequest, T.ContributionGraph];
    ListAchievements: [T.ListAchievementsRequest, T.ListAchievementsResponse];
    ListHomeFeed: [T.ListHomeFeedRequest, T.ListHomeFeedResponse];
  };
  SwarmService: {
    CreateSwarm: [T.CreateSwarmRequest, T.Swarm];
    GetSwarm: [T.SwarmRef, T.Swarm];
    ListRepoWork: [T.ListRepoWorkRequest, T.ListRepoWorkResponse];
    SetSwarmState: [T.SetSwarmStateRequest, T.Swarm];
    SetSwarmStatus: [T.SetSwarmStatusRequest, T.Swarm];
    AddSwarmTask: [T.AddSwarmTaskRequest, T.SwarmTask];
    StartSwarmTask: [T.StartSwarmTaskRequest, T.SwarmTask];
    FinishSwarmTask: [T.FinishSwarmTaskRequest, T.SwarmTask];
    SendSwarmMessage: [T.SendSwarmMessageRequest, T.AgentRunEvent];
    RequestSwarmJoin: [T.RequestSwarmJoinRequest, T.SwarmJoin];
    DecideSwarmJoin: [T.DecideSwarmJoinRequest, T.SwarmJoin];
  };
  SafetyService: {
    GetSafetyRecord: [T.SafetyRef, T.SafetyRecord];
    ListSafetyRecords: [T.ListSafetyRecordsRequest, T.ListSafetyRecordsResponse];
    ReportFinding: [T.ReportFindingRequest, T.SafetyFinding];
    ResolveFinding: [T.ResolveFindingRequest, T.SafetyFinding];
    SetVerdict: [T.SetVerdictRequest, T.SafetyRecord];
    RequestSafetyReview: [T.RequestSafetyReviewRequest, T.SafetyReview];
    SubmitSafetyReview: [T.SubmitSafetyReviewRequest, T.SafetyReview];
    RaiseConcern: [T.RaiseConcernRequest, T.SafetyConcern];
    ListSafetyAttention: [T.ListSafetyAttentionRequest, T.ListSafetyAttentionResponse];
    AcknowledgeSafetyAttention: [T.AcknowledgeSafetyAttentionRequest, T.SafetyAttention];
    StopEverything: [T.SetSafetyStopRequest, T.SafetyStop];
    ResumeEverything: [T.SetSafetyStopRequest, T.SafetyStop];
    SpotCheck: [T.SpotCheckRequest, T.SpotCheckResponse];
    GetSafetySummary: [T.GetSafetySummaryRequest, T.SafetySummary];
  };
  PostService: {
    CreatePost: [T.CreatePostRequest, T.Post];
    SetChangePost: [T.SetChangePostRequest, T.Post];
    ClearChangePost: [T.ClearChangePostRequest, T.ClearChangePostResponse];
    GetChangePost: [T.GetChangePostRequest, T.Post];
    BeginPostMediaUpload: [T.BeginPostMediaUploadRequest, T.BeginPostMediaUploadResponse];
    FinalizePostMedia: [T.FinalizePostMediaRequest, T.PostMedia];
    ApprovePost: [T.ApprovePostRequest, T.Post];
    WithdrawPost: [T.WithdrawPostRequest, T.Post];
    GetPost: [T.GetPostRequest, T.Post];
    ListRepoPosts: [T.ListRepoPostsRequest, T.ListRepoPostsResponse];
    LikePost: [T.LikePostRequest, T.Post];
    UnlikePost: [T.UnlikePostRequest, T.Post];
    RepostPost: [T.RepostPostRequest, T.Post];
    UndoRepost: [T.UndoRepostRequest, T.Post];
    ListFollowingFeed: [T.ListFollowingFeedRequest, T.ListFollowingFeedResponse];
    ListPostAdoptions: [T.ListPostAdoptionsRequest, T.ListPostAdoptionsResponse];
  };
  InsightsService: {
    GetLanguages: [T.GetLanguagesRequest, T.GetLanguagesResponse];
  };
  BillingService: {
    ListBillingPlans: [T.ListBillingPlansRequest, T.ListBillingPlansResponse];
    GetAccountPlan: [T.GetAccountPlanRequest, T.AccountPlan];
    ListSeats: [T.ListSeatsRequest, T.ListSeatsResponse];
    QueryUsage: [T.QueryUsageRequest, T.QueryUsageResponse];
    ChangePlan: [T.ChangePlanRequest, T.AccountPlan];
  };
  OpsService: {
    GetOpsHealth: [T.GetOpsHealthRequest, T.OpsHealth];
    GetClusterStatus: [T.GetClusterStatusRequest, T.ClusterStatus];
    ValidateOpsConfig: [T.ValidateOpsConfigRequest, T.ValidateOpsConfigResponse];
    GetEffectiveOpsConfig: [T.GetEffectiveOpsConfigRequest, T.EffectiveOpsConfig];
  };
  EnterpriseService: {
    GetEnterprise: [T.GetEnterpriseRequest, T.Enterprise];
    UpdateEnterprise: [T.UpdateEnterpriseRequest, T.Enterprise];
    ListEnterpriseOrgs: [T.ListEnterpriseOrgsRequest, T.ListEnterpriseOrgsResponse];
    ListEnterpriseOwners: [T.ListEnterpriseOwnersRequest, T.ListEnterpriseOwnersResponse];
    GetSamlConfig: [T.GetSamlConfigRequest, T.SamlConfig];
    UpdateSamlConfig: [T.UpdateSamlConfigRequest, T.SamlConfig];
    TestSamlConfig: [T.TestSamlConfigRequest, T.TestSamlConfigResponse];
    GetScimConfig: [T.GetScimConfigRequest, T.ScimConfig];
    UpdateScimConfig: [T.UpdateScimConfigRequest, T.ScimConfig];
    CreateScimToken: [T.CreateScimTokenRequest, T.CreateScimTokenResponse];
    ListVerifiedDomains: [T.ListVerifiedDomainsRequest, T.ListVerifiedDomainsResponse];
    AddVerifiedDomain: [T.AddVerifiedDomainRequest, T.VerifiedDomain];
    CheckVerifiedDomain: [T.CheckVerifiedDomainRequest, T.VerifiedDomain];
    DeleteVerifiedDomain: [T.DeleteVerifiedDomainRequest, T.DeleteVerifiedDomainResponse];
    GetTwoFactorRequirement: [T.GetTwoFactorRequirementRequest, T.TwoFactorRequirement];
    UpdateTwoFactorRequirement: [T.UpdateTwoFactorRequirementRequest, T.UpdateTwoFactorRequirementResponse];
    ListIpAllowList: [T.ListIpAllowListRequest, T.ListIpAllowListResponse];
    CreateIpAllowListEntry: [T.CreateIpAllowListEntryRequest, T.IpAllowEntry];
    DeleteIpAllowListEntry: [T.DeleteIpAllowListEntryRequest, T.DeleteIpAllowListEntryResponse];
    GetEnterprisePolicies: [T.GetEnterprisePoliciesRequest, T.EnterprisePolicies];
    UpdateEnterprisePolicies: [T.UpdateEnterprisePoliciesRequest, T.EnterprisePolicies];
  };
  TeamService: {
    ListTeams: [T.ListTeamsRequest, T.ListTeamsResponse];
    ListRepoTeams: [T.ListRepoTeamsRequest, T.ListRepoTeamsResponse];
    ListTeamMembers: [ListTeamMembersRequestJson, ListTeamMembersResponseJson];
  };
  AppService: {
    ListApps: [T.ListAppsRequest, T.ListAppsResponse];
  };
}

export type ServiceName = keyof Methods;
export type MethodName<S extends ServiceName> = keyof Methods[S] & string;
export type Req<S extends ServiceName, M extends MethodName<S>> = Methods[S][M] extends [infer I, unknown] ? I : never;
export type Res<S extends ServiceName, M extends MethodName<S>> = Methods[S][M] extends [unknown, infer O] ? O : never;

/** UI methods that stream, at the type level. Keep in step with STREAMING below. */
export type StreamingRoute = "EventService/Subscribe" | "QueryService/ReadPath" | "QueryService/Diff" | "WorkflowService/StreamLogs";

/** UI methods that stream. Everything else resolves once. */
export const STREAMING: ReadonlySet<string> = new Set(["EventService/Subscribe", "QueryService/ReadPath", "QueryService/Diff", "WorkflowService/StreamLogs"]);
