import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { useSharedModals } from "@/contexts/shared-modal";
import { useToast } from "@/contexts/toast";
import {
  GTaskEventStatusEnums,
  TaskDescStatusEnums,
  TaskGroupDesc,
  TaskTypeEnums,
} from "@/models/task";
import { InstanceService } from "@/services/instance";

// This download has its own archive validation and mirror fallbacks. Keep its
// lifecycle in the global provider so navigation cannot discard its progress.
export const useWandaDownloadTask = (
  updateGroupInfo: (group: TaskGroupDesc) => void
) => {
  const { t } = useTranslation();
  const toast = useToast();
  const { openSharedModal } = useSharedModals();
  const [tasks, setTasks] = useState<TaskGroupDesc[]>([]);
  const [isDownloading, setIsDownloading] = useState(false);
  const currentRun = useRef<{ group: string; cancelled: boolean } | null>(null);

  const updateTask = useCallback(
    (id: string, update: (group: TaskGroupDesc) => void) => {
      setTasks((previous) =>
        previous.map((group) => {
          if (group.taskGroup !== id) return group;
          const next = {
            ...group,
            taskDescs: group.taskDescs.map((task) => ({
              ...task,
              payload: { ...task.payload },
            })),
          };
          update(next);
          updateGroupInfo(next);
          return next;
        })
      );
    },
    [updateGroupInfo]
  );

  useEffect(
    () =>
      InstanceService.onWandaDownloadProgress((progress) => {
        const run = currentRun.current;
        if (!run || run.cancelled) return;
        updateTask(run.group, (group) => {
          const task = group.taskDescs[0];
          task.current = progress.current;
          task.total = progress.total ?? 0;
          task.speed = progress.speed;
          task.status = TaskDescStatusEnums.InProgress;
          if (
            ["resolving", "downloading", "verifying"].includes(progress.phase)
          ) {
            group.description = [
              t(`DownloadTasksPage.label.${progress.phase}`),
              progress.source,
            ]
              .filter(Boolean)
              .join(" · ");
          }
          if (progress.phase === "failed") {
            group.status = GTaskEventStatusEnums.Failed;
            group.description = undefined;
            task.status = TaskDescStatusEnums.Failed;
            task.reason = progress.message || undefined;
            group.reason = task.reason;
          }
          if (progress.phase === "cancelled") {
            run.cancelled = true;
            group.status = GTaskEventStatusEnums.Cancelled;
            task.status = TaskDescStatusEnums.Cancelled;
            task.speed = 0;
            group.description = undefined;
          }
        });
      }),
    [t, updateTask]
  );

  const start = useCallback(async () => {
    if (currentRun.current) return;
    const timestamp = Date.now();
    const run = { group: `wanda-modpack@${timestamp}`, cancelled: false };
    currentRun.current = run;
    setIsDownloading(true);
    const group: TaskGroupDesc = {
      taskGroup: run.group,
      status: GTaskEventStatusEnums.Started,
      description: t("DownloadTasksPage.label.resolving"),
      taskDescs: [
        {
          taskId: -timestamp,
          taskGroup: run.group,
          current: 0,
          total: 0,
          status: TaskDescStatusEnums.Waiting,
          payload: {
            taskType: TaskTypeEnums.Download,
            src: "",
            dest: "",
            filename: t("DownloadTasksPage.task.wanda-modpack"),
            sha1: "",
          },
        },
      ],
    };
    updateGroupInfo(group);
    setTasks((previous) => [group, ...previous]);
    try {
      const response = await InstanceService.downloadWandaModpack();
      if (run.cancelled) return;
      if (response.status !== "success") {
        throw new Error(response.details || response.message);
      }
      updateTask(run.group, (next) => {
        next.status = GTaskEventStatusEnums.Completed;
        next.description = undefined;
        const task = next.taskDescs[0];
        task.payload.dest = response.data;
        task.payload.filename =
          response.data.split(/[\\/]/).pop() || task.payload.filename;
      });
      openSharedModal("import-modpack", {
        path: response.data,
        modpackUpdateChannel:
          "https://gitee.com/Muzimimiao/BBGU-Minecraft-sever/raw/main/sjmcl-update.json",
      });
    } catch (error) {
      if (run.cancelled) return;
      const reason = error instanceof Error ? error.message : String(error);
      updateTask(run.group, (next) => {
        next.status = GTaskEventStatusEnums.Failed;
        next.description = undefined;
        next.reason = reason;
        next.taskDescs[0].status = TaskDescStatusEnums.Failed;
        next.taskDescs[0].reason = reason;
      });
      toast({ title: reason, status: "error" });
    } finally {
      currentRun.current = null;
      setIsDownloading(false);
    }
  }, [openSharedModal, t, toast, updateGroupInfo, updateTask]);

  const cancel = useCallback(async () => {
    const run = currentRun.current;
    if (!run || run.cancelled) return;
    run.cancelled = true;
    try {
      await InstanceService.cancelWandaModpack();
    } catch (error) {
      run.cancelled = false;
      toast({
        title: t("DownloadTasksPage.label.error"),
        description: String(error),
        status: "error",
      });
      return;
    }
    updateTask(run.group, (group) => {
      group.status = GTaskEventStatusEnums.Cancelled;
      group.description = undefined;
      group.taskDescs[0].status = TaskDescStatusEnums.Cancelled;
      group.taskDescs[0].speed = 0;
    });
  }, [t, toast, updateTask]);

  const clearHistory = useCallback(() => {
    setTasks((previous) =>
      previous.filter((group) => group.status === GTaskEventStatusEnums.Started)
    );
  }, []);

  return { tasks, isDownloading, start, cancel, clearHistory };
};
