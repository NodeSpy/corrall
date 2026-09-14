import type { PluginServerContext } from "@getpaseo/plugin/server";

import {
  disableRpc,
  enableRpc,
  loginCancelRpc,
  loginStartRpc,
  loginSubmitRpc,
  poolCreateRpc,
  poolUpdateRpc,
  priorityRpc,
  reloadRpc,
  statusRpc,
} from "./shared/contracts";
import * as handlers from "./server/handlers";

export default function contribute(server: PluginServerContext) {
  server.handle(statusRpc, handlers.status);
  server.handle(enableRpc, handlers.enable);
  server.handle(disableRpc, handlers.disable);
  server.handle(priorityRpc, handlers.priority);
  server.handle(reloadRpc, handlers.reload);
  server.handle(poolCreateRpc, handlers.poolCreate);
  server.handle(poolUpdateRpc, handlers.poolUpdate);
  server.handle(loginStartRpc, handlers.loginStart);
  server.handle(loginSubmitRpc, handlers.loginSubmit);
  server.handle(loginCancelRpc, handlers.loginCancel);
  return () => {};
}
