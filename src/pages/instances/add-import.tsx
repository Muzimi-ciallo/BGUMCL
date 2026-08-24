import {
  HStack,
  Icon,
  Menu,
  MenuButton,
  MenuItem,
  MenuList,
  Portal,
  Spinner,
  Text,
  VStack,
  useDisclosure,
} from "@chakra-ui/react";
import { open } from "@tauri-apps/plugin-dialog";
import { useRouter } from "next/router";
import { useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { LuArrowRight, LuCloudDownload, LuFolderPlus } from "react-icons/lu";
import {
  OptionItemGroup,
  OptionItemGroupProps,
} from "@/components/common/option-item";
import { CreateInstanceModal } from "@/components/modals/create-instance-modal";
import { DownloadGameServerModal } from "@/components/modals/download-game-server-modal";
import DownloadModpackModal from "@/components/modals/download-modpack-modal";
import { WandaDownloadProgressModal } from "@/components/modals/wanda-download-progress-modal";
import { useSharedModals } from "@/contexts/shared-modal";
import { useToast } from "@/contexts/toast";
import { InstanceService, WandaDownloadProgress } from "@/services/instance";

const AddAndImportInstancePage = () => {
  const { t } = useTranslation();
  const router = useRouter();
  const { openSharedModal } = useSharedModals();
  const toast = useToast();
  const [isDownloadingWanda, setIsDownloadingWanda] = useState(false);
  const [isWandaProgressVisible, setIsWandaProgressVisible] = useState(false);
  const [isCancellingWanda, setIsCancellingWanda] = useState(false);
  const wandaCancelledRef = useRef(false);
  const [wandaProgress, setWandaProgress] = useState<WandaDownloadProgress>({
    phase: "resolving",
    current: 0,
    total: null,
    speed: 0,
    source: "",
    message: null,
  });

  const handleDownloadWandaModpack = async () => {
    if (isDownloadingWanda) return;
    setIsDownloadingWanda(true);
    setIsWandaProgressVisible(true);
    setIsCancellingWanda(false);
    wandaCancelledRef.current = false;
    setWandaProgress({
      phase: "resolving",
      current: 0,
      total: null,
      speed: 0,
      source: "",
      message: null,
    });
    const stopProgressListener =
      InstanceService.onWandaDownloadProgress(setWandaProgress);
    try {
      const res = await InstanceService.downloadWandaModpack();
      if (res.status === "success") {
        openSharedModal("import-modpack", {
          path: res.data,
          modpackUpdateChannel:
            "https://gitee.com/Muzimimiao/BBGU-Minecraft-sever/raw/main/sjmcl-update.json",
        });
      } else if (!wandaCancelledRef.current) {
        toast({
          title: res.message,
          description: res.details,
          status: "error",
        });
      }
    } catch (error) {
      if (!wandaCancelledRef.current) {
        toast({ title: String(error), status: "error" });
      }
    } finally {
      stopProgressListener();
      setIsWandaProgressVisible(false);
      setIsCancellingWanda(false);
      setIsDownloadingWanda(false);
    }
  };

  const handleCancelWandaModpack = async () => {
    if (!isDownloadingWanda || isCancellingWanda) return;
    wandaCancelledRef.current = true;
    setIsCancellingWanda(true);
    try {
      await InstanceService.cancelWandaModpack();
    } catch (error) {
      wandaCancelledRef.current = false;
      setIsCancellingWanda(false);
      toast({ title: String(error), status: "error" });
    }
  };

  const {
    isOpen: isCreateInstanceModalOpen,
    onOpen: onOpenCreateInstanceModal,
    onClose: onCloseCreateInstanceModal,
  } = useDisclosure();
  const {
    isOpen: isModpackMenuOpen,
    onOpen: onOpenModpackMenu,
    onClose: onCloseModpackMenu,
  } = useDisclosure();
  const {
    isOpen: isDownloadModpackModalOpen,
    onOpen: onOpenDownloadModpackModal,
    onClose: onCloseDownloadModpackModal,
  } = useDisclosure();
  const {
    isOpen: isDownloadGameServerModalOpen,
    onOpen: onOpenDownloadGameServerModal,
    onClose: onCloseDownloadGameServerModal,
  } = useDisclosure();

  const handleImportModpackFromDisk = async () => {
    let filePath = await open({
      multiple: false,
      filters: [
        {
          name: t("General.dialog.filterName.modpack"),
          extensions: ["zip", "mrpack"],
        },
      ],
    });
    if (filePath) {
      openSharedModal("import-modpack", {
        path: filePath,
      });
    }
  };

  const addAndImportOptions: Record<string, () => void> = {
    new: onOpenCreateInstanceModal,
    modpack: onOpenModpackMenu,
    manageDirs: () => router.push("/settings/global-game"),
  };

  const moreOptions: Record<string, () => void> = {
    server: onOpenDownloadGameServerModal,
    wanda: handleDownloadWandaModpack,
  };

  const modpackOperations = [
    {
      icon: LuFolderPlus,
      label: t("AddAndImportInstancePage.modpackOperations.fromdisk"),
      onClick: () => {
        handleImportModpackFromDisk();
      },
    },
    {
      icon: LuCloudDownload,
      label: t("AddAndImportInstancePage.modpackOperations.download"),
      onClick: () => {
        onOpenDownloadModpackModal();
      },
    },
  ];

  const ModpackMenu = () => {
    return (
      <Menu isOpen={isModpackMenuOpen} onClose={onCloseModpackMenu}>
        <MenuButton>
          <Icon as={LuArrowRight} boxSize={3.5} mr="5px" />
        </MenuButton>
        <Portal>
          <MenuList>
            {modpackOperations.map((item) => (
              <MenuItem key={item.label} fontSize="xs" onClick={item.onClick}>
                <HStack>
                  <item.icon />
                  <Text>{item.label}</Text>
                </HStack>
              </MenuItem>
            ))}
          </MenuList>
        </Portal>
      </Menu>
    );
  };

  const optionGroups: OptionItemGroupProps[] = [
    {
      title: t("AllInstancesPage.button.addAndImport"),
      items: Object.keys(addAndImportOptions).map((key) => ({
        title: t(`AddAndImportInstancePage.addAndImportOptions.${key}.title`),
        description: t(
          `AddAndImportInstancePage.addAndImportOptions.${key}.description`
        ),
        children:
          key === "modpack" ? (
            <ModpackMenu />
          ) : (
            <Icon as={LuArrowRight} boxSize={3.5} mr="5px" />
          ),
        isFullClickZone: true,
        onClick: addAndImportOptions[key],
      })),
    },
    {
      title: t("AddAndImportInstancePage.moreOptions.title"),
      items: Object.keys(moreOptions).map((key) => ({
        title: t(`AddAndImportInstancePage.moreOptions.${key}.title`),
        description: t(
          `AddAndImportInstancePage.moreOptions.${key}.description`
        ),
        children:
          key === "wanda" && isDownloadingWanda ? (
            <Spinner size="sm" mr="5px" />
          ) : (
            <Icon as={LuArrowRight} boxSize={3.5} mr="5px" />
          ),
        isFullClickZone: true,
        onClick: moreOptions[key],
      })),
    },
  ];

  return (
    <>
      <VStack w="100%" spacing={4}>
        {optionGroups.map((group, index) => (
          <OptionItemGroup w="100%" {...group} key={index} />
        ))}
      </VStack>
      <CreateInstanceModal
        isOpen={isCreateInstanceModalOpen}
        onClose={onCloseCreateInstanceModal}
      />
      <DownloadModpackModal
        isOpen={isDownloadModpackModalOpen}
        onClose={onCloseDownloadModpackModal}
      />
      <DownloadGameServerModal
        isOpen={isDownloadGameServerModalOpen}
        onClose={onCloseDownloadGameServerModal}
      />
      <WandaDownloadProgressModal
        isOpen={isWandaProgressVisible}
        progress={wandaProgress}
        isCancelling={isCancellingWanda}
        onClose={() => setIsWandaProgressVisible(false)}
        onCancel={handleCancelWandaModpack}
      />
    </>
  );
};

export default AddAndImportInstancePage;
