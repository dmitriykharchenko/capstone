import { store } from "../../store";

export const selectGraphStatus = store.createSelector((state) => state.graphExplorer.status);
export const selectGraphNodes = store.createSelector((state) => state.graphExplorer.nodes);
export const selectGraphEdges = store.createSelector((state) => state.graphExplorer.edges);
export const selectGraphNodeId = store.createSelector(
  (state) => state.graphExplorer.selectedNodeId,
);
export const selectGraphMessage = store.createSelector((state) => state.graphExplorer.message);