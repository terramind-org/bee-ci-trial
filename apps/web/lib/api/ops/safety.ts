/** Asked-against-done evidence, independent reviews and human oversight. */
import { SafetyService } from "../../gen/bee/v1/safety_pb";
import { pass, type Ops } from "./shared";

export const safetyOps = {
  SafetyService: {
    GetSafetyRecord: pass(SafetyService.method.getSafetyRecord),
    ListSafetyRecords: pass(SafetyService.method.listSafetyRecords),
    ReportFinding: pass(SafetyService.method.reportFinding),
    ResolveFinding: pass(SafetyService.method.resolveFinding),
    SetVerdict: pass(SafetyService.method.setVerdict),
    RequestSafetyReview: pass(SafetyService.method.requestSafetyReview),
    SubmitSafetyReview: pass(SafetyService.method.submitSafetyReview),
    RaiseConcern: pass(SafetyService.method.raiseConcern),
    ListSafetyAttention: pass(SafetyService.method.listSafetyAttention),
    AcknowledgeSafetyAttention: pass(SafetyService.method.acknowledgeSafetyAttention),
    StopEverything: pass(SafetyService.method.stopEverything),
    ResumeEverything: pass(SafetyService.method.resumeEverything),
    SpotCheck: pass(SafetyService.method.spotCheck),
    GetSafetySummary: pass(SafetyService.method.getSafetySummary),
  },
} satisfies Ops<"SafetyService">;
