import {
  Alert,
  AlertIcon,
  Badge,
  Box,
  Button,
  Checkbox,
  HStack,
  Modal,
  ModalBody,
  ModalCloseButton,
  ModalContent,
  ModalFooter,
  ModalHeader,
  ModalOverlay,
  ModalProps,
  Progress,
  Text,
  Tooltip,
  VStack,
  Wrap,
} from "@chakra-ui/react";
import { open } from "@tauri-apps/plugin-dialog";
import { useRouter } from "next/router";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { LuFolderOpen, LuScanLine } from "react-icons/lu";
import { useLauncherConfig } from "@/contexts/config";
import { useToast } from "@/contexts/toast";
import {
  ModpackIntegrityReport,
  ModpackIntegrityStatus,
} from "@/models/instance/misc";
import { GTaskEventStatusEnums } from "@/models/task";
import { InstanceService } from "@/services/instance";
import { TaskService } from "@/services/task";

interface Props extends Omit<ModalProps, "children"> {
  instanceId: string;
  instanceName: string;
}

const statusColors: Record<ModpackIntegrityStatus, string> = {
  valid: "green",
  missing: "red",
  corrupted: "orange",
  disabled: "gray",
  optional: "gray",
  unverified: "yellow",
};

const repairable = (status: ModpackIntegrityStatus) =>
  status === "missing" || status === "corrupted";

const ModpackIntegrityModal: React.FC<Props> = ({
  instanceId,
  instanceName,
  ...modalProps
}) => {
  const { t } = useTranslation();
  const { config } = useLauncherConfig();
  const toast = useToast();
  const router = useRouter();
  const [path, setPath] = useState("");
  const [report, setReport] = useState<ModpackIntegrityReport>();
  const [selected, setSelected] = useState<string[]>([]);
  const [scanning, setScanning] = useState(false);
  const [repairing, setRepairing] = useState(false);
  const [onlyIssues, setOnlyIssues] = useState(true);
  const [error, setError] = useState("");
  const [activeDownloads, setActiveDownloads] = useState(false);
  const busy = scanning || repairing;

  useEffect(() => {
    let disposed = false;
    const refresh = async () => {
      const response = await TaskService.retrieveProgressiveTaskList();
      if (!disposed && response.status === "success") {
        setActiveDownloads(
          response.data.some(
            (task) =>
              task.status === GTaskEventStatusEnums.Started ||
              task.status === GTaskEventStatusEnums.Stopped
          )
        );
      }
    };
    refresh();
    const unlisten = TaskService.onTaskGroupUpdate(refresh);
    return () => {
      disposed = true;
      unlisten();
    };
  }, []);
  const candidates =
    report?.entries.filter((entry) => repairable(entry.status)) || [];
  const entries =
    report?.entries.filter(
      (entry) =>
        !onlyIssues || repairable(entry.status) || entry.status === "unverified"
    ) || [];

  const handleChoose = async () => {
    try {
      const chosen = await open({
        multiple: false,
        filters: [
          {
            name: t("ModpackIntegrity.archiveFilter"),
            extensions: ["zip", "mrpack"],
          },
        ],
      });
      if (typeof chosen === "string") {
        setPath(chosen);
        setReport(undefined);
        setSelected([]);
        setError("");
      }
    } catch (err) {
      setError(String(err));
    }
  };

  const handleScan = async () => {
    setScanning(true);
    setError("");
    setReport(undefined);
    setSelected([]);
    try {
      const response = await InstanceService.scanModpackIntegrity(
        instanceId,
        path
      );
      if (response.status === "error") {
        setError(response.details);
        return;
      }
      setReport(response.data);
      setSelected(
        response.data.entries
          .filter((entry) => repairable(entry.status))
          .map((entry) => entry.id)
      );
    } finally {
      setScanning(false);
    }
  };

  const handleCache = async () => {
    try {
      const response = await InstanceService.openDownloadCache();
      if (response.status === "error") throw new Error(response.details);
    } catch (err) {
      toast({
        title: t("ModpackIntegrity.cacheError"),
        description: String(err),
        status: "error",
      });
    }
  };

  const handleRepair = async () => {
    if (!report) return;
    setRepairing(true);
    setError("");
    try {
      const response = await InstanceService.repairModpackIntegrity(
        instanceId,
        report.scanId,
        selected
      );
      if (response.status === "error") {
        setError(response.details);
        setReport(undefined);
        setSelected([]);
        return;
      }
      toast({
        title: t("ModpackIntegrity.repairStarted", { ...response.data }),
        status: "success",
      });
      modalProps.onClose();
      if (response.data.queued > 0) router.push("/downloads");
    } finally {
      setRepairing(false);
    }
  };

  return (
    <Modal
      {...modalProps}
      size="2xl"
      closeOnOverlayClick={!busy}
      closeOnEsc={!repairing}
      scrollBehavior="inside"
    >
      <ModalOverlay />
      <ModalContent>
        <ModalHeader>{t("ModpackIntegrity.title")}</ModalHeader>
        <ModalCloseButton isDisabled={repairing} />
        <ModalBody>
          <VStack align="stretch" spacing={4}>
            <Text fontWeight="medium">{instanceName}</Text>
            <Text fontSize="sm" className="secondary-text">
              {t("ModpackIntegrity.description")}
            </Text>
            <HStack flexWrap="wrap">
              <Button
                size="sm"
                leftIcon={<LuFolderOpen />}
                onClick={handleChoose}
                isDisabled={busy}
              >
                {t("ModpackIntegrity.choose")}
              </Button>
              <Button size="sm" variant="ghost" onClick={handleCache}>
                {t("ModpackIntegrity.openCache")}
              </Button>
            </HStack>
            {path && (
              <Text fontSize="xs" wordBreak="break-all">
                {path}
              </Text>
            )}
            {error && (
              <Alert status="error" borderRadius="md">
                <AlertIcon />
                <Text fontSize="sm" wordBreak="break-word">
                  {error}
                </Text>
              </Alert>
            )}
            {scanning && (
              <Box>
                <Progress isIndeterminate size="xs" />
                <Text mt={2} fontSize="sm">
                  {t("ModpackIntegrity.scanning")}
                </Text>
              </Box>
            )}
            {report && (
              <>
                <Text fontWeight="medium">{report.packName}</Text>
                <Wrap spacing={2}>
                  {(Object.keys(statusColors) as ModpackIntegrityStatus[]).map(
                    (status) => (
                      <Badge key={status} colorScheme={statusColors[status]}>
                        {t(`ModpackIntegrity.status.${status}`)}{" "}
                        {
                          report.entries.filter(
                            (entry) => entry.status === status
                          ).length
                        }
                      </Badge>
                    )
                  )}
                </Wrap>
                <HStack justify="space-between" flexWrap="wrap">
                  <Checkbox
                    size="sm"
                    isChecked={
                      selected.length === candidates.length &&
                      candidates.length > 0
                    }
                    isIndeterminate={
                      selected.length > 0 && selected.length < candidates.length
                    }
                    isDisabled={repairing || candidates.length === 0}
                    onChange={(event) =>
                      setSelected(
                        event.target.checked
                          ? candidates.map((entry) => entry.id)
                          : []
                      )
                    }
                  >
                    {t("ModpackIntegrity.selectAll")}
                  </Checkbox>
                  <Checkbox
                    size="sm"
                    isChecked={onlyIssues}
                    onChange={(event) => setOnlyIssues(event.target.checked)}
                  >
                    {t("ModpackIntegrity.onlyIssues")}
                  </Checkbox>
                </HStack>
                <VStack
                  align="stretch"
                  spacing={0}
                  maxH="320px"
                  overflowY="auto"
                >
                  {entries.map((entry) => (
                    <HStack
                      key={entry.id}
                      py={2}
                      borderBottomWidth="1px"
                      align="start"
                      spacing={3}
                    >
                      <Checkbox
                        mt={1}
                        aria-label={entry.path}
                        isChecked={selected.includes(entry.id)}
                        isDisabled={repairing || !repairable(entry.status)}
                        onChange={(event) =>
                          setSelected((ids) =>
                            event.target.checked
                              ? [...ids, entry.id]
                              : ids.filter((id) => id !== entry.id)
                          )
                        }
                      />
                      <Box flex={1} minW={0}>
                        <Text fontSize="sm" wordBreak="break-all">
                          {entry.path}
                        </Text>
                        <Text fontSize="xs" className="secondary-text">
                          {t(`ModpackIntegrity.source.${entry.source}`)}
                        </Text>
                        {entry.detail && (
                          <Tooltip label={entry.detail}>
                            <Text
                              fontSize="xs"
                              color="orange.500"
                              noOfLines={2}
                            >
                              {entry.detail}
                            </Text>
                          </Tooltip>
                        )}
                      </Box>
                      <Badge
                        flexShrink={0}
                        colorScheme={statusColors[entry.status]}
                      >
                        {t(`ModpackIntegrity.status.${entry.status}`)}
                      </Badge>
                    </HStack>
                  ))}
                  {entries.length === 0 && (
                    <Text py={4} textAlign="center" fontSize="sm">
                      {t(
                        report.entries.length === 0
                          ? "ModpackIntegrity.noMods"
                          : "ModpackIntegrity.noIssues"
                      )}
                    </Text>
                  )}
                </VStack>
                {activeDownloads && (
                  <Text fontSize="xs" color="orange.500">
                    {t("ModpackIntegrity.waitDownloads")}
                  </Text>
                )}
              </>
            )}
          </VStack>
        </ModalBody>
        <ModalFooter gap={2} flexWrap="wrap">
          <Button
            variant="ghost"
            onClick={modalProps.onClose}
            isDisabled={repairing}
          >
            {t("General.close")}
          </Button>
          <Button
            leftIcon={<LuScanLine />}
            onClick={handleScan}
            isLoading={scanning}
            isDisabled={!path || repairing}
          >
            {t(report ? "ModpackIntegrity.rescan" : "ModpackIntegrity.scan")}
          </Button>
          {report && (
            <Button
              colorScheme={config.appearance.theme.primaryColor}
              onClick={handleRepair}
              isLoading={repairing}
              isDisabled={selected.length === 0 || activeDownloads}
            >
              {t("ModpackIntegrity.repair", { count: selected.length })}
            </Button>
          )}
        </ModalFooter>
      </ModalContent>
    </Modal>
  );
};

export default ModpackIntegrityModal;
