import createBackendHostsSource, {
  createWorkspace,
  type Workspace,
} from "@pohunek/client-core";
import { workspaceStores, type WorkspaceStores } from "./store-adapter";

export interface BrowserWorkspace {
  readonly workspace: Workspace;
  readonly stores: WorkspaceStores;
}

let instance: BrowserWorkspace | undefined;

/** Returns the single workspace used by this browser tab. */
export function getBrowserWorkspace(): BrowserWorkspace {
  if (typeof window === "undefined") {
    throw new Error("the Pohunek workspace is only available in a browser");
  }
  instance ??= createBrowserWorkspace(window.location.origin);
  return instance;
}

function createBrowserWorkspace(baseUrl: string): BrowserWorkspace {
  const workspace = createWorkspace({
    baseUrl,
    hosts: createBackendHostsSource(baseUrl),
  });
  return { workspace, stores: workspaceStores(workspace) };
}
