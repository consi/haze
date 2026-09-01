/** Minimal shape shared by local groups and replication group previews. */
export interface HierarchicalGroup {
  uuid: string;
  parent_uuid: string | null;
  display_name: string;
  depth: number;
}

/**
 * Build stable breadcrumb labels and a shallow-to-deep comparator for a
 * snapshot of groups. The UUID tie-breaker keeps ordering deterministic when
 * duplicate display names exist in different branches.
 */
export function groupHierarchy<T extends HierarchicalGroup>(groups: readonly T[]) {
  const byUuid = new Map(groups.map((g) => [g.uuid, g]));
  const breadcrumbs = new Map<string, string>();

  function breadcrumb(group: T): string {
    const cached = breadcrumbs.get(group.uuid);
    if (cached != null) return cached;

    const parts: string[] = [];
    const visited = new Set<string>();
    let current: T | undefined = group;
    while (current && !visited.has(current.uuid)) {
      visited.add(current.uuid);
      parts.unshift(current.display_name);
      current = current.parent_uuid ? byUuid.get(current.parent_uuid) : undefined;
    }
    const label = parts.join(' > ');
    breadcrumbs.set(group.uuid, label);
    return label;
  }

  function compare(a: T, b: T): number {
    return (
      a.depth - b.depth ||
      breadcrumb(a).localeCompare(breadcrumb(b), undefined, { sensitivity: 'base' }) ||
      a.uuid.localeCompare(b.uuid)
    );
  }

  return { breadcrumb, compare };
}

export function sortGroupsShallowFirst<T extends HierarchicalGroup>(groups: readonly T[]): T[] {
  const { compare } = groupHierarchy(groups);
  return [...groups].sort(compare);
}
