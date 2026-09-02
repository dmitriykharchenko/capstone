import { call, put, race, take } from "typed-redux-saga";
import { eventChannel, type EventChannel } from "redux-saga";
import { navigateTo, routeChanged } from "../router-slice";
import {
  parseRoute,
  serializeRoute,
  type AppRoute,
  type RouteLocation,
} from "../router-types";

export interface RouteNavigation {
  readLocation(): RouteLocation;
  push(url: string): void;
  replace(url: string): void;
  subscribe(listener: (location: RouteLocation) => void): () => void;
}

export const browserRouteNavigation: RouteNavigation = {
  readLocation: () => ({
    pathname: window.location.pathname,
    search: window.location.search,
  }),
  push: (url) => window.history.pushState(null, "", url),
  replace: (url) => window.history.replaceState(null, "", url),
  subscribe: (listener) => {
    const onPopState = () => listener(browserRouteNavigation.readLocation());
    window.addEventListener("popstate", onPopState);
    return () => window.removeEventListener("popstate", onPopState);
  },
};

export function createPopstateChannel(
  navigation: RouteNavigation,
): EventChannel<RouteLocation> {
  return eventChannel<RouteLocation>((emit) => navigation.subscribe(emit));
}

function* synchronizeLocation(
  navigation: RouteNavigation,
  location: RouteLocation,
) {
  const route = parseRoute(location);
  const canonicalUrl = serializeRoute(route);
  if (`${location.pathname}${location.search}` !== canonicalUrl) {
    yield* call(navigation.replace, canonicalUrl);
  }
  yield* put(routeChanged(route));
}

function* navigate(navigation: RouteNavigation, route: AppRoute) {
  const url = serializeRoute(route);
  const location = yield* call(navigation.readLocation);
  if (`${location.pathname}${location.search}` !== url) {
    yield* call(navigation.push, url);
  }
  yield* put(routeChanged(route));
}

export function* routerSaga(
  navigation: RouteNavigation = browserRouteNavigation,
) {
  const channel = yield* call(createPopstateChannel, navigation);
  try {
    const initialLocation = yield* call(navigation.readLocation);
    yield* call(synchronizeLocation, navigation, initialLocation);

    while (true) {
      const { intent, popstate } = yield* race({
        intent: take(navigateTo),
        popstate: take(channel),
      });
      if (intent) {
        yield* call(navigate, navigation, intent.payload[0]);
      } else if (popstate) {
        yield* call(synchronizeLocation, navigation, popstate);
      }
    }
  } finally {
    channel.close();
  }
}