import type { ButtonHTMLAttributes, HTMLAttributes, ReactNode } from "react";
import type { LucideIcon } from "lucide-react";

export type Theme = "light" | "dark";
export function Button({
  variant = "secondary",
  className = "",
  ...props
}: ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: "primary" | "secondary" | "danger" | "ghost";
}) {
  const styles = {
    primary: "border-signal bg-signal text-on-signal hover:brightness-110",
    secondary: "border-line bg-raised text-ink hover:border-signal/60 hover:bg-panel",
    danger: "border-critical/40 bg-critical/10 text-critical hover:bg-critical/20",
    ghost: "border-transparent bg-transparent text-dim hover:bg-raised hover:text-ink",
  };
  return (
    <button
      className={`inline-flex min-h-10 items-center justify-center gap-2 rounded-xl border px-3.5 py-2 text-xs font-bold transition ${styles[variant]} ${className}`}
      {...props}
    />
  );
}
export function Panel({ className = "", ...props }: HTMLAttributes<HTMLElement>) {
  return (
    <section
      className={`min-w-0 rounded-panel border border-line bg-panel shadow-panel ${className}`}
      {...props}
    />
  );
}
export function SectionTitle({
  eyebrow,
  title,
  aside,
}: {
  eyebrow: string;
  title: string;
  aside?: ReactNode;
}) {
  return (
    <div className="mb-6 flex items-start justify-between gap-4">
      <div>
        <p className="mb-1 font-mono text-[10px] font-bold tracking-[.22em] text-signal">
          {eyebrow}
        </p>
        <h2 className="m-0 text-xl font-bold tracking-[-.03em] text-ink">{title}</h2>
      </div>
      {aside}
    </div>
  );
}
export function Field({
  label,
  children,
  className = "",
}: {
  label: string;
  children: ReactNode;
  className?: string;
}) {
  return (
    <label className={`field ${className}`}>
      {label}
      {children}
    </label>
  );
}
export function StatusBadge({
  tone = "neutral",
  children,
}: {
  tone?: "neutral" | "live" | "warn" | "danger";
  children: ReactNode;
}) {
  const tones = {
    neutral: "border-line bg-raised text-dim",
    live: "border-signal/35 bg-signal/10 text-signal",
    warn: "border-warn/35 bg-warn/10 text-warn",
    danger: "border-critical/35 bg-critical/10 text-critical",
  };
  return (
    <span
      className={`inline-flex items-center gap-2 rounded-full border px-3 py-1.5 font-mono text-[10px] font-bold ${tones[tone]}`}
    >
      <i className="size-1.5 rounded-full bg-current shadow-[0_0_8px_currentColor]" />
      {children}
    </span>
  );
}
export function MetricCard({
  label,
  value,
  unit,
  icon: Icon,
  tone = "signal",
  help,
}: {
  label: string;
  value: string | number;
  unit?: string;
  icon: LucideIcon;
  tone?: "signal" | "info" | "warn" | "violet";
  help?: string;
}) {
  const tones = {
      signal: "text-signal bg-signal/10",
      info: "text-info bg-info/10",
      warn: "text-warn bg-warn/10",
      violet: "text-violet bg-violet/10",
    },
    active = label === "ACTIVE";
  return (
    <article
      title={
        help ??
        (active
          ? "왼쪽은 1초 간격 순간값, 오른쪽은 최근 1분의 최대 동시 연결 수입니다."
          : undefined)
      }
      className="rounded-2xl border border-line bg-raised/60 p-4 transition hover:-translate-y-0.5 hover:border-signal/30"
    >
      <div className="flex items-center justify-between">
        <span className="text-[10px] font-bold tracking-[.12em] text-dim">
          {active ? "ACTIVE · 현재 / 1분 최대" : label}
        </span>
        <span className={`grid size-8 place-items-center rounded-lg ${tones[tone]}`}>
          <Icon size={15} />
        </span>
      </div>
      <strong className="mono-numbers mt-4 block text-2xl font-semibold tracking-[-.04em] text-ink">
        {value}
        <small className="ml-1.5 text-[10px] font-medium text-dim">{unit}</small>
      </strong>
    </article>
  );
}
