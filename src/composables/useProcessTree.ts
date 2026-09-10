import type { ProcessInfo } from "../types";

export interface ProcessNode extends ProcessInfo {
  children?: ProcessNode[];
  _depth?: number;
}

export interface TableRow extends ProcessInfo {
  _depth: number;
  _hasChildren: boolean;
}

/** Build parent-child tree from flat process list */
export function buildProcessTree(all: ProcessInfo[]): ProcessNode[] {
  // Filter out processes that cannot have their affinity changed
  const filtered = all.filter(p => !p.access_denied);
  const byPid = new Map<number, ProcessNode>();
  for (const p of filtered) {
    byPid.set(p.pid, { ...p, children: [], _depth: 0 });
  }
  const roots: ProcessNode[] = [];
  for (const node of byPid.values()) {
    const parent = byPid.get(node.parent_pid);
    if (parent && parent.pid !== node.pid) {
      parent.children!.push(node);
    } else {
      roots.push(node);
    }
  }
  const sortChildren = (nodes: ProcessNode[]) => {
    nodes.sort(
      (a, b) =>
        a.name.localeCompare(b.name, undefined, { numeric: true, sensitivity: "base" }) ||
        a.pid - b.pid,
    );
    for (const n of nodes) if (n.children?.length) sortChildren(n.children);
  };
  sortChildren(roots);
  return roots;
}

/** Flatten tree to row list based on expanded pids */
export function flattenTree(roots: ProcessNode[], expandedSet: Set<number>): TableRow[] {
  const out: TableRow[] = [];
  const walk = (nodes: ProcessNode[], depth: number) => {
    for (const n of nodes) {
      const hasChildren = (n.children?.length ?? 0) > 0;
      out.push({ ...(n as ProcessInfo), _depth: depth, _hasChildren: hasChildren });
      if (hasChildren && expandedSet.has(n.pid)) {
        walk(n.children!, depth + 1);
      }
    }
  };
  walk(roots, 0);
  return out;
}

/** Build search whitelist: matched pids + their ancestor chain */
export function computeSearchWhitelist(
  all: ProcessInfo[],
  q: string,
): { set: Set<number>; needExpandPids: Set<number> } {
  const lowerQ = q.toLowerCase();
  const isMatch = (p: ProcessInfo): boolean => {
    const lowerName = p.name.toLowerCase();
    return lowerName.includes(lowerQ) || String(p.pid) === q || String(p.parent_pid) === q;
  };
  const byPid = new Map<number, ProcessInfo>();
  for (const p of all) byPid.set(p.pid, p);
  const whitelist = new Set<number>();
  const needExpand = new Set<number>();
  const mark = (startPid: number) => {
    let curPid: number | undefined = startPid;
    let isFirst = true;
    while (curPid != null && !whitelist.has(curPid)) {
      whitelist.add(curPid);
      if (!isFirst) needExpand.add(curPid);
      isFirst = false;
      const p = byPid.get(curPid);
      const parentPid = p?.parent_pid;
      if (parentPid == null || parentPid === curPid) break;
      if (!byPid.has(parentPid)) break;
      curPid = parentPid;
    }
  };
  for (const p of all) if (isMatch(p)) mark(p.pid);
  return { set: whitelist, needExpandPids: needExpand };
}

export function filterTree(nodes: ProcessNode[], whitelist: Set<number>): ProcessNode[] {
  const out: ProcessNode[] = [];
  for (const n of nodes) {
    if (!whitelist.has(n.pid)) continue;
    const filteredChildren = n.children ? filterTree(n.children, whitelist) : [];
    out.push({ ...n, children: filteredChildren, _depth: n._depth });
  }
  return out;
}

export function countChildren(processes: ProcessInfo[], pid: number): number {
  let cnt = 0;
  for (const p of processes) if (p.parent_pid === pid) cnt++;
  return cnt;
}

/** Toggle a node's expand state (expanded = not collapsed by default) */
export function toggleTreeNodeExpand(
  pid: number,
  item: ProcessInfo,
  expandedPids: number[],
): void {
  if (!(item as TableRow)._hasChildren) return;
  const idx = expandedPids.indexOf(pid);
  if (idx >= 0) {
    expandedPids.splice(idx, 1);
  } else {
    expandedPids.push(pid);
  }
}
