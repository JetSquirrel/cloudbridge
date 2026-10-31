// Extended Support charges: RDS, Aurora and EKS bill extra, per vCPU or
// per cluster hour, for an engine or Kubernetes version past the end of
// standard support. Upgrading stops the charge; it is not a discount you
// can buy out of.
import type { Finding, Input, Rule } from "../../types/cloudbridge-insights";

export const rule: Rule = {
  title: "Paying for Extended Support",
  label: "EXTENDED SUPPORT",
  explanation:
    "Databases and clusters on a version past standard support, billed extra for it. " +
    "Upgrade the engine or Kubernetes version to stop the surcharge.",
  version: "1.0.0",
  author: "JetSquirrel",
  providers: ["AWS"],
  apiVersion: 1,
};

const EXTENDED = /extended support/i;

export function evaluate(input: Input): Finding[] {
  const findings: Finding[] = [];
  for (const r of input.billed) {
    const lines = r.lines.filter((l) => EXTENDED.test(l.description ?? ""));
    const cost = lines.reduce((sum, l) => sum + l.cost, 0);
    if (cost <= 0) continue;
    const share = r.cost > 0 ? Math.round((100 * cost) / r.cost) : 100;
    findings.push({
      id: r.id,
      cost,
      evidence: `Extended Support is ${share}% of what it cost this period`,
    });
  }
  return findings;
}
