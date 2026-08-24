import {
  Button,
  HStack,
  Modal,
  ModalBody,
  ModalCloseButton,
  ModalContent,
  ModalFooter,
  ModalHeader,
  ModalOverlay,
  Progress,
  Text,
  VStack,
} from "@chakra-ui/react";
import { WandaDownloadProgress } from "@/services/instance";
import { formatByteSize } from "@/utils/string";

interface WandaDownloadProgressModalProps {
  isOpen: boolean;
  progress: WandaDownloadProgress;
  isCancelling: boolean;
  onClose: () => void;
  onCancel: () => void;
}

const phaseText: Record<WandaDownloadProgress["phase"], string> = {
  resolving: "获取整合包信息",
  downloading: "下载整合包",
  verifying: "校验整合包",
  completed: "下载完成",
  failed: "下载失败",
  cancelled: "已取消",
};

export const WandaDownloadProgressModal: React.FC<
  WandaDownloadProgressModalProps
> = ({ isOpen, progress, isCancelling, onClose, onCancel }) => {
  const percent =
    progress.total && progress.total > 0
      ? Math.min(100, (progress.current / progress.total) * 100)
      : undefined;

  return (
    <Modal
      isOpen={isOpen}
      onClose={onClose}
      isCentered
      closeOnEsc
      closeOnOverlayClick
    >
      <ModalOverlay />
      <ModalContent>
        <ModalHeader>下载湾大整合包</ModalHeader>
        <ModalCloseButton />
        <ModalBody pb={6}>
          <VStack align="stretch" spacing={3}>
            <HStack justify="space-between">
              <Text>{phaseText[progress.phase]}</Text>
              {percent !== undefined && <Text>{percent.toFixed(1)}%</Text>}
            </HStack>
            <Progress
              value={percent}
              isIndeterminate={percent === undefined}
              colorScheme="blue"
            />
            <HStack justify="space-between" color="gray.500" fontSize="sm">
              <Text>
                {formatByteSize(progress.current)}
                {progress.total ? ` / ${formatByteSize(progress.total)}` : ""}
              </Text>
              <Text>
                {progress.speed > 0
                  ? `${formatByteSize(progress.speed)}/s`
                  : ""}
              </Text>
            </HStack>
            {progress.source && (
              <Text color="gray.500" fontSize="sm">
                来源：{progress.source}
              </Text>
            )}
            {progress.message && (
              <Text color="red.500" fontSize="sm">
                {progress.message}
              </Text>
            )}
          </VStack>
        </ModalBody>
        <ModalFooter>
          <Button
            colorScheme="red"
            variant="outline"
            onClick={onCancel}
            isLoading={isCancelling}
            isDisabled={
              isCancelling ||
              progress.phase === "completed" ||
              progress.phase === "failed"
            }
          >
            {isCancelling ? "取消中..." : "取消下载"}
          </Button>
        </ModalFooter>
      </ModalContent>
    </Modal>
  );
};
