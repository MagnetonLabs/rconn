// WebAuthn JSON <-> browser objects. Options arrive as webauthn-rs serialises them
// (`{publicKey: …}` with base64url binary fields); credentials go back in the same encoding.

function fromB64url(s: string): ArrayBuffer {
  const b = atob(s.replace(/-/g, '+').replace(/_/g, '/') + '==='.slice((s.length + 3) % 4));
  const out = new Uint8Array(b.length);
  for (let i = 0; i < b.length; i++) out[i] = b.charCodeAt(i);
  return out.buffer;
}

function toB64url(buf: ArrayBuffer | ArrayBufferView | null | undefined): string | null {
  if (!buf) return null;
  const bytes = buf instanceof ArrayBuffer ? new Uint8Array(buf) : new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength);
  let s = '';
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

function decode(o: Record<string, unknown>): Record<string, unknown> {
  const pk = { ...o };
  if (typeof pk.challenge === 'string') pk.challenge = fromB64url(pk.challenge);
  if (pk.user && typeof pk.user === 'object') {
    const u = { ...(pk.user as Record<string, unknown>) };
    if (typeof u.id === 'string') u.id = fromB64url(u.id);
    pk.user = u;
  }
  for (const key of ['excludeCredentials', 'allowCredentials']) {
    const list = pk[key];
    if (Array.isArray(list)) {
      pk[key] = list.map((c: Record<string, unknown>) => ({
        ...c,
        id: typeof c.id === 'string' ? fromB64url(c.id) : c.id,
      }));
    }
  }
  return pk;
}

function encode(cred: PublicKeyCredential): unknown {
  const r = cred.response as AuthenticatorAttestationResponse & AuthenticatorAssertionResponse;
  const response: Record<string, string | null> = { clientDataJSON: toB64url(r.clientDataJSON) };
  if ('attestationObject' in r && r.attestationObject) response.attestationObject = toB64url(r.attestationObject);
  if ('authenticatorData' in r && r.authenticatorData) {
    response.authenticatorData = toB64url(r.authenticatorData);
    response.signature = toB64url(r.signature);
    response.userHandle = toB64url(r.userHandle);
  }
  return {
    id: cred.id,
    rawId: toB64url(cred.rawId),
    type: cred.type,
    response,
    extensions: cred.getClientExtensionResults?.() ?? {},
  };
}

export const passkeysSupported = () =>
  typeof window !== 'undefined' && typeof window.PublicKeyCredential === 'function' && window.isSecureContext;

export async function createCredential(options: { publicKey: Record<string, unknown> }): Promise<unknown> {
  const cred = (await navigator.credentials.create({
    publicKey: decode(options.publicKey) as unknown as PublicKeyCredentialCreationOptions,
  })) as PublicKeyCredential | null;
  if (!cred) throw new Error('No passkey was created.');
  return encode(cred);
}

export async function getCredential(options: { publicKey: Record<string, unknown> }): Promise<unknown> {
  const cred = (await navigator.credentials.get({
    publicKey: decode(options.publicKey) as unknown as PublicKeyCredentialRequestOptions,
  })) as PublicKeyCredential | null;
  if (!cred) throw new Error('No passkey was chosen.');
  return encode(cred);
}
