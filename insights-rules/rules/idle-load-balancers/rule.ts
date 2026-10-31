// Load balancers billed by the hour that served no traffic this period.
// An Application or Network Load Balancer bills about $16 a month just for
// existing; the capacity-unit lines are what it charges for work done.
import type { Billed, Finding, Input, Rule } from "../../types/cloudbridge-insights";

export const rule: Rule = {
  title: "Idle load balancers",
  label: "IDLE LOAD BALANCERS",
  explanation:
    "Load balancers billed by the hour that used no capacity units this period: " +
    "nothing was routed through them. Delete the ones no DNS record points at.",
  version: "1.0.0",
  author: "JetSquirrel",
  providers: ["AWS"],
  apiVersion: 1,
};

const HOURS = /LoadBalancer-hour/i;
const CAPACITY = /capacity unit/i;

/** Hours billed, once at least a day's worth, with no capacity used. */
function idleHours(r: Billed): number | null {
  const hours = r.lines
    .filter((l) => HOURS.test(l.description ?? ""))
    .reduce((sum, l) => sum + l.quantity, 0);
  const used = r.lines
    .filter((l) => CAPACITY.test(l.description ?? ""))
    .reduce((sum, l) => sum + l.quantity, 0);
  return hours >= 24 && used === 0 ? hours : null;
}

export function evaluate(input: Input): Finding[] {
  const findings: Finding[] = [];
  for (const r of input.billed) {
    const hours = idleHours(r);
    if (hours !== null) {
      findings.push({
        id: r.id,
        evidence: `Billed for ${Math.round(hours)} hours with no capacity units used`,
      });
    }
  }
  return findings;
}
