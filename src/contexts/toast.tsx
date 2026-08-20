import {
  Center,
  ToastId,
  UseToastOptions,
  useToast as chakraUseToast,
  useColorModeValue,
} from "@chakra-ui/react";
import React, {
  ReactNode,
  createContext,
  useCallback,
  useContext,
  useRef,
} from "react";
import { BeatLoader } from "react-spinners";

interface ToastContextProviderProps {
  children: ReactNode;
}

interface ToastContextType {
  (options: UseToastOptions): ToastId;
}

const ToastContext = createContext<ToastContextType | null>(null);

// Cap simultaneous toasts so a burst of notifications (e.g. many download
// tasks finishing / failing at once) does not cover the bottom-left of the UI
// and block clicks on other buttons.
const MAX_ACTIVE_TOASTS = 3;

export const ToastContextProvider: React.FC<ToastContextProviderProps> = ({
  children,
}) => {
  const chakraToast = chakraUseToast();
  const toastVariant = useColorModeValue("left-accent", "solid");
  const activeToastIds = useRef<Set<ToastId>>(new Set());

  const customToast: ToastContextType = useCallback(
    (options) => {
      // Loading toasts are persistent (duration null) and are kept until the
      // caller closes them; only auto-evict regular toasts beyond the cap.
      if (options.status !== "loading") {
        while (activeToastIds.current.size >= MAX_ACTIVE_TOASTS) {
          const oldest = activeToastIds.current.values().next().value;
          if (oldest === undefined) break;
          chakraToast.close(oldest);
          activeToastIds.current.delete(oldest);
        }
      }

      let id: ToastId;
      const toast = chakraToast({
        position: "bottom-left",
        duration: options.status === "loading" ? null : 3000,
        icon:
          options.status === "loading" ? (
            <Center h="100%" mt={0.5}>
              <BeatLoader size={4} />
            </Center>
          ) : null,
        variant: toastVariant,
        isClosable: true,
        containerStyle: {
          minWidth: "2xs",
          userSelect: "none",
        },
        onCloseComplete: () => {
          activeToastIds.current.delete(id);
        },
        ...options,
      });
      id = toast;
      activeToastIds.current.add(id);
      return id;
    },
    [chakraToast, toastVariant]
  );

  return (
    <ToastContext.Provider value={customToast}>
      {children}
    </ToastContext.Provider>
  );
};

export const useToast = (): ToastContextType => {
  const context = useContext(ToastContext);
  if (!context)
    throw new Error("useToast must be used within a ToastContextProvider");
  return context;
};
