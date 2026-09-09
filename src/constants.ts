/** CCD bar color palette (max 8 CCDs, high contrast) */
export const CCD_COLORS = [
  "#42A5F5", "#66BB6A", "#FFA726", "#EF5350",
  "#AB47BC", "#26C6DA", "#FFEE58", "#8D6E63",
];

/** Vuetify data-table SortItem type */
export type SortItem = { key: string; order?: boolean | "asc" | "desc" };

/** View mode for the process table */
export type ViewMode = "flat" | "tree";

/** CPU scale mode for display */
export type CpuScaleMode = "per-core" | "overall";
