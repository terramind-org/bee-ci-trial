/** Swarm coordination and standalone runs share the repository work feed. */
import { SwarmService } from "../../gen/bee/v1/swarms_pb";
import { pass, type Ops } from "./shared";

export const swarmOps = {
  SwarmService: {
    CreateSwarm: pass(SwarmService.method.createSwarm),
    GetSwarm: pass(SwarmService.method.getSwarm),
    ListRepoWork: pass(SwarmService.method.listRepoWork),
    SetSwarmState: pass(SwarmService.method.setSwarmState),
    SetSwarmStatus: pass(SwarmService.method.setSwarmStatus),
    AddSwarmTask: pass(SwarmService.method.addSwarmTask),
    StartSwarmTask: pass(SwarmService.method.startSwarmTask),
    FinishSwarmTask: pass(SwarmService.method.finishSwarmTask),
    SendSwarmMessage: pass(SwarmService.method.sendSwarmMessage),
    RequestSwarmJoin: pass(SwarmService.method.requestSwarmJoin),
    DecideSwarmJoin: pass(SwarmService.method.decideSwarmJoin),
  },
} satisfies Ops<"SwarmService">;
