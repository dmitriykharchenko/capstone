import { createAction } from "@augmentcode/themis/utils/store/create-action";
import { createReducer } from "@augmentcode/themis/utils/store/create-reducer";
import type { StoreAction, StoreReducerFunction } from "@augmentcode/themis/types";
import { initialRouterState, type AppRoute, type RouterState } from "./router-types";

export const navigateTo = createAction<[route: AppRoute]>("router/navigate");
export const routeChanged = createAction<[route: AppRoute]>("router/changed");

export const routerReducer: StoreReducerFunction<RouterState> =
  createReducer<RouterState>(initialRouterState).with(
    routeChanged,
    (state: RouterState, action: StoreAction<[AppRoute]>) => ({
      ...state,
      activeRoute: action.payload[0],
      initialized: true,
    }),
  );