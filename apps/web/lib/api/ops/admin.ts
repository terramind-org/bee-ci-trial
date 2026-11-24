/** The admin console: server operations (ops.proto) and the enterprise (enterprise.proto). */
import { EnterpriseService } from "../../gen/bee/v1/enterprise_pb";
import { OpsService } from "../../gen/bee/v1/ops_pb";
import { pass, type PartialOps } from "./shared";

export const adminOps = {
  OpsService: {
    GetOpsHealth: pass(OpsService.method.getOpsHealth),
    GetClusterStatus: pass(OpsService.method.getOpsClusterStatus),
    ValidateOpsConfig: pass(OpsService.method.validateOpsConfig),
    GetEffectiveOpsConfig: pass(OpsService.method.getEffectiveOpsConfig),
  },
  EnterpriseService: {
    GetEnterprise: pass(EnterpriseService.method.getEnterprise),
    UpdateEnterprise: pass(EnterpriseService.method.updateEnterprise),
    ListEnterpriseOrgs: pass(EnterpriseService.method.listEnterpriseOrgs),
    ListEnterpriseOwners: pass(EnterpriseService.method.listEnterpriseOwners),
    GetSamlConfig: pass(EnterpriseService.method.getSamlSsoConfig),
    UpdateSamlConfig: pass(EnterpriseService.method.updateSamlSsoConfig),
    TestSamlConfig: pass(EnterpriseService.method.testSamlSsoConfig),
    GetScimConfig: pass(EnterpriseService.method.getScimConfig),
    UpdateScimConfig: pass(EnterpriseService.method.updateScimConfig),
    CreateScimToken: pass(EnterpriseService.method.createScimToken),
    ListVerifiedDomains: pass(EnterpriseService.method.listVerifiedDomains),
    AddVerifiedDomain: pass(EnterpriseService.method.addVerifiedDomain),
    CheckVerifiedDomain: pass(EnterpriseService.method.checkVerifiedDomain),
    DeleteVerifiedDomain: pass(EnterpriseService.method.deleteVerifiedDomain),
    GetTwoFactorRequirement: pass(EnterpriseService.method.getTwoFactorRequirement),
    UpdateTwoFactorRequirement: pass(EnterpriseService.method.updateTwoFactorRequirement),
    ListIpAllowList: pass(EnterpriseService.method.listIpAllowListEntries),
    CreateIpAllowListEntry: pass(EnterpriseService.method.createIpAllowListEntry),
    DeleteIpAllowListEntry: pass(EnterpriseService.method.deleteIpAllowListEntry),
    GetEnterprisePolicies: pass(EnterpriseService.method.getEnterprisePolicies),
    UpdateEnterprisePolicies: pass(EnterpriseService.method.updateEnterprisePolicies),
  },
} satisfies PartialOps<"OpsService" | "EnterpriseService">;
