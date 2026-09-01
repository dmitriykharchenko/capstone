import type { PaperInfo } from "../../lib/api";

export interface PapersState {
  items: PaperInfo[];
  loading: boolean;
  error: string | null;
  selectedId: string | null;
  pdfUrl: string | null;
  pdfLoading: boolean;
}

export const initialPapersState: PapersState = {
  items: [],
  loading: false,
  error: null,
  selectedId: null,
  pdfUrl: null,
  pdfLoading: false,
};
