export const getDownloadProgress = (
  current: number,
  total: number
): number | undefined => {
  if (!Number.isFinite(total) || total <= 0) return undefined;
  if (!Number.isFinite(current)) return 0;
  return Math.min(100, Math.max(0, (current * 100) / total));
};

export const isWandaTaskGroup = (taskGroup: string): boolean =>
  taskGroup.startsWith("wanda-modpack@");
