import { writable, type Readable } from "svelte/store";
import { parseRoute, routePath, type AppRoute } from "./routes";

export interface NavigateOptions {
  readonly replace?: boolean;
}

export interface HistoryRouter {
  readonly current: Readable<AppRoute>;
  href(route: AppRoute): string;
  navigate(route: AppRoute, options?: NavigateOptions): void;
  close(): void;
}

/** Creates a browser History API router. Unknown paths safely render the workspace. */
export function createHistoryRouter(): HistoryRouter {
  if (typeof window === "undefined") {
    throw new Error("the history router is only available in a browser");
  }

  const current = writable(currentBrowserRoute());
  const onPopState = (): void => {
    current.set(currentBrowserRoute());
  };
  window.addEventListener("popstate", onPopState);

  return {
    current,
    href: routePath,
    navigate(route, options): void {
      const path = routePath(route);
      if (options?.replace === true) {
        window.history.replaceState(null, "", path);
      } else {
        window.history.pushState(null, "", path);
      }
      current.set(route);
    },
    close(): void {
      window.removeEventListener("popstate", onPopState);
    },
  };
}

function currentBrowserRoute(): AppRoute {
  return parseRoute(window.location.pathname) ?? { kind: "workspace" };
}
