import { motion, AnimatePresence } from "motion/react";
import { AlertCircle, AlertTriangle, X } from "lucide-react";

export type ToastType = "error" | "warning";

export interface ToastItem {
  id: number;
  type: ToastType;
  message: string;
  createdAt: number;
  duration: number;
}

interface ToastContainerProps {
  toasts: ToastItem[];
  onDismiss: (id: number) => void;
}

const colorMap: Record<ToastType, { strip: string; icon: string; bg: string }> = {
  error: { strip: "bg-red-500", icon: "text-red-500", bg: "bg-red-500/10" },
  warning: { strip: "bg-amber-500", icon: "text-amber-500", bg: "bg-amber-500/10" },
};

const iconMap: Record<ToastType, typeof AlertCircle> = {
  error: AlertCircle,
  warning: AlertTriangle,
};

export default function ToastContainer({ toasts, onDismiss }: ToastContainerProps) {
  return (
    <div className="fixed bottom-4 left-1/2 -translate-x-1/2 z-50 flex flex-col gap-2 w-[90vw] max-w-sm pointer-events-none">
      <AnimatePresence mode="popLayout">
        {toasts.map((toast) => {
          const Icon = iconMap[toast.type];
          const colors = colorMap[toast.type];
          return (
            <motion.div
              key={toast.id}
              layout
              initial={{ opacity: 0, y: 20 }}
              animate={{ opacity: 1, y: 0 }}
              exit={{ opacity: 0, y: 20 }}
              transition={{ type: "spring", stiffness: 400, damping: 30 }}
              className="pointer-events-auto flex items-start gap-3 rounded-xl border border-border-main overflow-hidden shadow-lg"
              style={{ backgroundColor: "var(--bg-card)" }}
            >
              <div className={`w-1.5 self-stretch shrink-0 ${colors.strip}`} />
              <div className={`flex items-start gap-2 py-3 pr-3 ${colors.bg}`}>
                <Icon className={`w-4 h-4 mt-0.5 shrink-0 ${colors.icon}`} />
                <p className="text-xs leading-relaxed" style={{ color: "var(--text-main)" }}>
                  {toast.message}
                </p>
              </div>
              <button
                onClick={() => onDismiss(toast.id)}
                className="ml-auto p-3 shrink-0 opacity-50 hover:opacity-100 transition-opacity cursor-pointer"
                style={{ color: "var(--text-muted)" }}
              >
                <X className="w-3.5 h-3.5" />
              </button>
            </motion.div>
          );
        })}
      </AnimatePresence>
    </div>
  );
}