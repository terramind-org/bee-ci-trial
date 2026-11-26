"use client";

import { useState } from "react";
import type { EnterpriseScope, SamlConfig, TestSamlConfigResponse } from "@/lib/api/types";
import { CopyAs } from "../copy-as";
import { Badge, Button, Field, Input, Select, Textarea } from "../ui";
import { Time } from "../time";
import { ActionError, Check, ConfirmButton, CopyValue, actionsRow, changed, formClass, useAdminAction } from "./kit";

const SIGNATURES = ["", "rsa-sha256", "rsa-sha512", "ecdsa-sha256"];
const DIGESTS = ["", "sha256", "sha512"];

/** SAML single sign-on: the identity provider, the service-provider values to give it, and a test. */
export function SamlForm({ scope, config }: { scope: EnterpriseScope; config: SamlConfig }) {
  const initial = {
    enabled: config.enabled,
    enforced: config.enforced,
    idpMetadataUrl: config.idpMetadataUrl,
    idpMetadataXml: config.idpMetadataXml,
    idpEntityId: config.idpEntityId,
    idpSsoUrl: config.idpSsoUrl,
    idpCertificatePem: config.idpCertificatePem,
    signatureMethod: config.signatureMethod,
    digestMethod: config.digestMethod,
    sessionLifetimeSeconds: config.sessionLifetimeSeconds,
  };
  const [f, setF] = useState(initial);
  const set = <K extends keyof typeof initial>(k: K, v: (typeof initial)[K]) => setF((x) => ({ ...x, [k]: v }));
  const { run, busy, error } = useAdminAction();
  const [tested, setTested] = useState<TestSamlConfigResponse | null>(null);
  const patch = changed(initial, f);
  const request = { scope, ...patch };
  const dirty = Object.keys(patch).length > 0;
  const testRequest = { scope, idpMetadataUrl: f.idpMetadataUrl || undefined, idpMetadataXml: f.idpMetadataXml || undefined };
  const enforcing = f.enforced && !config.enforced;

  const save = () => run("updateSamlConfig", request, "SAML saved");

  return (
    <form
      className={formClass}
      onSubmit={async (e) => {
        e.preventDefault();
        if (!enforcing) await save();
      }}
    >
      <div className="flex flex-col gap-1.5">
        <CopyValue label="Entity ID" value={config.spEntityId} />
        <CopyValue label="ACS URL" value={config.spAcsUrl} />
      </div>
      <p className="text-xs text-ink-3">Give these two values to your identity provider.{config.lastTestedAt ? <> Last tested <Time iso={config.lastTestedAt} />.</> : " Not tested yet."}</p>
      <div className="flex flex-wrap gap-x-6 gap-y-2">
        <Check id="saml-enabled" label="Enabled" checked={f.enabled} onChange={(v) => set("enabled", v)} />
        <Check id="saml-enforced" label="Enforced" hint="Everyone must sign in through SAML." checked={f.enforced} onChange={(v) => set("enforced", v)} />
      </div>
      <Field label="Metadata URL" htmlFor="saml-md-url" hint="Where the identity provider publishes its metadata.">
        <Input id="saml-md-url" type="url" value={f.idpMetadataUrl} onChange={(e) => set("idpMetadataUrl", e.target.value)} />
      </Field>
      <Field label="Metadata XML" htmlFor="saml-md-xml" hint="Paste instead of a URL when the provider is not reachable from here.">
        <Textarea id="saml-md-xml" rows={3} spellCheck={false} className="mono text-xs" value={f.idpMetadataXml} onChange={(e) => set("idpMetadataXml", e.target.value)} />
      </Field>
      <div className="grid grid-cols-1 gap-3.5 md:grid-cols-2">
        <Field label="Identity provider entity ID" htmlFor="saml-idp-entity">
          <Input id="saml-idp-entity" value={f.idpEntityId} onChange={(e) => set("idpEntityId", e.target.value)} />
        </Field>
        <Field label="Single sign-on URL" htmlFor="saml-idp-sso">
          <Input id="saml-idp-sso" type="url" value={f.idpSsoUrl} onChange={(e) => set("idpSsoUrl", e.target.value)} />
        </Field>
      </div>
      <Field label="Certificate (PEM)" htmlFor="saml-cert" hint={config.idpCertificateExpiresAt ? <>Expires <Time iso={config.idpCertificateExpiresAt} />.</> : undefined}>
        <Textarea id="saml-cert" rows={3} spellCheck={false} className="mono text-xs" value={f.idpCertificatePem} onChange={(e) => set("idpCertificatePem", e.target.value)} />
      </Field>
      <div className="grid grid-cols-1 gap-3.5 md:grid-cols-3">
        <Field label="Signature method" htmlFor="saml-sig">
          <Select id="saml-sig" value={f.signatureMethod} onChange={(e) => set("signatureMethod", e.target.value)}>
            {[...new Set([...SIGNATURES, f.signatureMethod])].map((v) => (
              <option key={v} value={v}>
                {v || "Server default"}
              </option>
            ))}
          </Select>
        </Field>
        <Field label="Digest method" htmlFor="saml-digest">
          <Select id="saml-digest" value={f.digestMethod} onChange={(e) => set("digestMethod", e.target.value)}>
            {[...new Set([...DIGESTS, f.digestMethod])].map((v) => (
              <option key={v} value={v}>
                {v || "Server default"}
              </option>
            ))}
          </Select>
        </Field>
        <Field label="Session lifetime (seconds)" htmlFor="saml-session">
          <Input id="saml-session" type="number" min={0} value={f.sessionLifetimeSeconds} onChange={(e) => set("sessionLifetimeSeconds", Number(e.target.value) || 0)} />
        </Field>
      </div>
      <ActionError error={error} />
      {tested ? (
        <div role="status" data-testid="saml-test-result" className="rounded-md border border-line px-3 py-2.5 text-sm">
          <p className="flex items-center gap-2">
            <Badge tone={tested.metadataOk ? "ok" : "bad"}>{tested.metadataOk ? "Metadata ok" : "Metadata has problems"}</Badge>
            {tested.testSignInUrl ? (
              <a href={tested.testSignInUrl} className="link" target="_blank" rel="noreferrer">
                Try a test sign-in
              </a>
            ) : null}
          </p>
          {tested.problems.length ? (
            <ul className="mt-1.5 list-disc pl-5 text-ink-2">
              {tested.problems.map((p) => (
                <li key={p}>{p}</li>
              ))}
            </ul>
          ) : null}
        </div>
      ) : null}
      <div className={actionsRow}>
        {enforcing ? (
          <ConfirmButton size="md" variant="primary" label="Save" title="Enforce SAML sign-in?" description="From now on everyone in this scope has to sign in through your identity provider. Make sure the test passes and you can sign in yourself." confirmLabel="Enforce and save" disabled={busy || !dirty} onConfirm={async () => void (await save())} />
        ) : (
          <Button type="submit" variant="primary" disabled={busy || !dirty}>
            Save
          </Button>
        )}
        <CopyAs op={{ service: "EnterpriseService", method: "UpdateSamlConfig", request, label: "Update SAML" }} />
        <Button
          disabled={busy || (!f.idpMetadataUrl && !f.idpMetadataXml)}
          onClick={async () => {
            const res = await run("testSamlConfig", testRequest, "SAML test finished", { refresh: false });
            if (res) setTested(res.data);
          }}
        >
          Test
        </Button>
        <CopyAs op={{ service: "EnterpriseService", method: "TestSamlConfig", request: testRequest, label: "Test SAML" }} />
      </div>
    </form>
  );
}
