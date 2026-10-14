import type { PaperInfo } from "./api";

export interface PaperListEntry {
  paper: PaperInfo;
  authors: string[];
  topics: string[];
}

export function paperListEntries(papers: PaperInfo[]): PaperListEntry[] {
  return papers.map((paper) => ({ paper, authors: paper.authors, topics: paper.topics }));
}

export function filterPaperList(entries: PaperListEntry[], query: string): PaperListEntry[] {
  const normalized = query.trim().toLocaleLowerCase();
  if (!normalized) return entries;
  return entries.filter(({ paper, authors, topics }) =>
    [paper.title ?? "", ...authors, ...topics].some((value) =>
      value.toLocaleLowerCase().includes(normalized),
    ),
  );
}