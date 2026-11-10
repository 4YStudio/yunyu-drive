import type { IconName } from "@/lib/icons";

export interface MenuItem {
  id: string;
  label: string;
  icon?: IconName;
  danger?: boolean;
  disabled?: boolean;
  hint?: string;
  separatorBefore?: boolean;
}
