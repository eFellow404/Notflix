function groupByNumericField(episodes, field) {
  const buckets = new Map();
  const unassigned = [];
  for (const ep of episodes) {
    const val = ep[field];
    if (val !== null && val !== undefined) {
      if (!buckets.has(val)) buckets.set(val, []);
      buckets.get(val).push(ep);
    } else {
      unassigned.push(ep);
    }
  }
  const sortedKeys = [...buckets.keys()].sort((a, b) => a - b);
  return { buckets, sortedKeys, unassigned };
}

export function buildEpisodeGroups(episodes) {
  const list = episodes || [];
  const main = list.filter((e) => !e.is_extra);
  const extras = list.filter((e) => e.is_extra);
  const groups = [];

  const hasSeasons = main.some((e) => e.season !== null && e.season !== undefined);
  const hasDiscs = !hasSeasons && main.some((e) => e.disc !== null && e.disc !== undefined);

  if (hasSeasons) {
    const { buckets, sortedKeys, unassigned } = groupByNumericField(main, "season");
    for (const s of sortedKeys) {
      const eps = buckets.get(s);
      const distinctDiscs = new Set(
        eps.filter((e) => e.disc !== null && e.disc !== undefined).map((e) => e.disc)
      );
      groups.push({
        key: `season:${s}`,
        kind: "season",
        season: s,
        label: s === 0 ? "Specials" : `Season ${s}`,
        shortLabel: s === 0 ? "Specials" : `S${s}`,
        showDiscDividers: distinctDiscs.size >= 2,
        episodes: eps,
      });
    }
    if (unassigned.length > 0) {
      groups.push({
        key: "other",
        kind: "other",
        label: "Other Episodes",
        shortLabel: "Other",
        showDiscDividers: false,
        episodes: unassigned,
      });
    }
  } else if (hasDiscs) {
    const { buckets, sortedKeys, unassigned } = groupByNumericField(main, "disc");
    for (const d of sortedKeys) {
      groups.push({
        key: `disc:${d}`,
        kind: "disc",
        disc: d,
        label: `Disc ${d}`,
        shortLabel: `Disc ${d}`,
        showDiscDividers: false,
        episodes: buckets.get(d),
      });
    }
    if (unassigned.length > 0) {
      groups.push({
        key: "other",
        kind: "other",
        label: "Other Episodes",
        shortLabel: "Other",
        showDiscDividers: false,
        episodes: unassigned,
      });
    }
  } else if (main.length > 0) {
    groups.push({
      key: "episodes",
      kind: "flat",
      label: "Episodes",
      shortLabel: "Episodes",
      showDiscDividers: false,
      episodes: main,
    });
  }

  if (extras.length > 0) {
    groups.push({
      key: "extras",
      kind: "extras",
      label: "Extras",
      shortLabel: "Extras",
      showDiscDividers: false,
      episodes: extras,
    });
  }

  return groups;
}

export function findGroupKeyForEpisode(groups, episodeId) {
  if (!episodeId) return groups[0]?.key || null;
  const found = groups.find((g) => g.episodes.some((e) => e.id === episodeId));
  return found ? found.key : groups[0]?.key || null;
}

export function compactEpisodeBadge(ep, groupKind, isGlobalList) {
  if (ep.is_extra) return "EXTRA";
  if (isGlobalList || groupKind === "other" || groupKind === "flat") {
    return ep.display_code;
  }
  if (groupKind === "season" && ep.episode_number !== null && ep.episode_number !== undefined) {
    return `E${String(ep.episode_number).padStart(2, "0")}`;
  }
  if (groupKind === "disc" && ep.episode_number !== null && ep.episode_number !== undefined) {
    return `T${String(ep.episode_number).padStart(2, "0")}`;
  }
  return ep.display_code;
}

export function describeEpisodePosition(ep, linkedList, episodeGroups = []) {
  if (!ep || !linkedList) return "";
  const total = linkedList.length;
  const idx = (ep.index ?? 0) + 1;
  if (ep.is_extra) return `Extra · #${idx} of ${total}`;

  if (ep.season !== null && ep.season !== undefined) {
    const sLabel = ep.season === 0 ? "Specials" : `Season ${ep.season}`;
    const group = episodeGroups.find((g) => g.key === `season:${ep.season}`);
    if (group) {
      const localPos = group.episodes.findIndex((e) => e.id === ep.id) + 1;
      return `${sLabel} · Episode ${localPos} of ${group.episodes.length}`;
    }
    return `${sLabel} · Episode ${idx} of ${total}`;
  }

  if (ep.disc !== null && ep.disc !== undefined) {
    const group = episodeGroups.find((g) => g.key === `disc:${ep.disc}`);
    if (group) {
      const localPos = group.episodes.findIndex((e) => e.id === ep.id) + 1;
      return `Disc ${ep.disc} · Track ${localPos} of ${group.episodes.length}`;
    }
    return `Disc ${ep.disc}`;
  }

  return `Episode ${idx} of ${total}`;
}
