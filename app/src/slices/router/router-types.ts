export type RouteName = "chat" | "history" | "papers" | "evals";

export interface ChatRoute {
  name: "chat";
  sessionId?: string;
  exchangeId?: string;
}

export interface HistoryRoute {
  name: "history";
  query?: string;
}

export interface PapersRoute {
  name: "papers";
  query?: string;
}

export interface EvalsRoute {
  name: "evals";
}

export type AppRoute = ChatRoute | HistoryRoute | PapersRoute | EvalsRoute;

export interface RouterState {
  activeRoute: AppRoute;
  initialized: boolean;
}

export interface RouteLocation {
  pathname: string;
  search: string;
}

export const defaultRoute: ChatRoute = { name: "chat" };

export const initialRouterState: RouterState = {
  activeRoute: defaultRoute,
  initialized: false,
};

function optionalParameter(params: URLSearchParams, name: string): string | undefined {
  const value = params.get(name);
  return value === null || value.length === 0 ? undefined : value;
}

export function parseRoute(location: RouteLocation): AppRoute {
  const params = new URLSearchParams(location.search);
  switch (location.pathname) {
    case "/chat":
      return {
        name: "chat",
        sessionId: optionalParameter(params, "sessionId"),
        exchangeId: optionalParameter(params, "exchangeId"),
      };
    case "/history":
      return { name: "history", query: optionalParameter(params, "q") };
    case "/papers":
      return { name: "papers", query: optionalParameter(params, "q") };
    case "/evals":
      return { name: "evals" };
    default:
      return defaultRoute;
  }
}

export function serializeRoute(route: AppRoute): string {
  const params = new URLSearchParams();
  if (route.name === "chat") {
    if (route.sessionId) params.set("sessionId", route.sessionId);
    if (route.exchangeId) params.set("exchangeId", route.exchangeId);
  } else if (route.name !== "evals" && route.query) {
    params.set("q", route.query);
  }
  const search = params.toString();
  return `/${route.name}${search ? `?${search}` : ""}`;
}

export function routeForName(name: RouteName): AppRoute {
  return { name };
}