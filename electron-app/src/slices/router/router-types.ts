export type RouteName = "chat" | "history" | "papers" | "graph" | "evals";

export interface ChatRoute {
  name: "chat";
  sessionId?: string;
  exchangeId?: number;
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

export interface GraphRoute {
  name: "graph";
}

export type AppRoute = ChatRoute | HistoryRoute | PapersRoute | GraphRoute | EvalsRoute;

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

function positiveIntegerParameter(
  params: URLSearchParams,
  name: string,
): number | undefined {
  const raw = optionalParameter(params, name);
  if (!raw || !/^[0-9]+$/.test(raw)) return undefined;
  const value = Number(raw);
  return Number.isSafeInteger(value) && value > 0 ? value : undefined;
}

export function parseRoute(location: RouteLocation): AppRoute {
  const params = new URLSearchParams(location.search);
  switch (location.pathname) {
    case "/chat":
      return {
        name: "chat",
        sessionId: optionalParameter(params, "sessionId"),
        exchangeId: positiveIntegerParameter(params, "exchangeId"),
      };
    case "/history":
      return { name: "history", query: optionalParameter(params, "q") };
    case "/papers":
      return { name: "papers", query: optionalParameter(params, "q") };
    case "/graph":
      return { name: "graph" };
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
    if (route.exchangeId) params.set("exchangeId", String(route.exchangeId));
  } else if ((route.name === "history" || route.name === "papers") && route.query) {
    params.set("q", route.query);
  }
  const search = params.toString();
  return `/${route.name}${search ? `?${search}` : ""}`;
}

export function routeForName(name: RouteName): AppRoute {
  return { name };
}