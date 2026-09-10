import { ArrowRightIcon, CheckCircleIcon } from "@phosphor-icons/react";
import type { ReactNode } from "react";

export function WorkflowStep({
  title,
  ready,
  children,
}: {
  title: string;
  ready: boolean;
  children: ReactNode;
}) {
  const Icon = ready ? CheckCircleIcon : ArrowRightIcon;
  return (
    <div className="flex gap-3 rounded-lg border border-surface-700/60 bg-surface-800/35 p-3">
      <Icon
        weight={ready ? "fill" : "bold"}
        className={
          ready
            ? "mt-0.5 h-5 w-5 shrink-0 text-success-text"
            : "mt-0.5 h-5 w-5 shrink-0 text-surface-500"
        }
      />
      <div className="min-w-0 flex-1">
        <h4 className="text-sm font-medium text-surface-100">{title}</h4>
        <div className="mt-1 text-sm text-surface-400">{children}</div>
      </div>
    </div>
  );
}
