/**
 * mwe-mcp MCP client — the TypeScript port of the reference client
 * (`agents-bridges/_harness/mwe_client.py`).
 *
 * Speaks the server's Streamable-HTTP endpoint directly (stateless, no
 * Mcp-Session-Id): one JSON-RPC `tools/call` POST per invocation, with a
 * Bearer JWT and — when the call speaks for a delegated human — the
 * `X-MWE-Act-As` header. Media bytes travel out of band through
 * `POST <origin>/media` (multipart) and ride the turn as catalog ids.
 *
 * Lives host-side. The consumer token never enters an agent container:
 * nanoclaw refuses a session spec carrying a secret-shaped env value
 * (`src/drivers/types.ts`, `isSecretShaped`), so the container reaches the
 * memory through the host action in `turn.ts` instead of holding a
 * credential of its own.
 */

export class MweError extends Error {
  readonly code: string | undefined;
  readonly data: unknown;

  constructor(message: string, options: { code?: string; data?: unknown } = {}) {
    super(message);
    this.name = 'MweError';
    this.code = options.code;
    this.data = options.data;
  }
}

export interface MediaUpload {
  bytes: Uint8Array;
  filename: string;
  /** The catalog enum: `photo | video | audio | doc`. */
  kind: string;
  mime?: string;
  caption?: string;
  description?: string;
}

/**
 * One logical MCP connection: endpoint + token + a fixed act-as identity.
 *
 * A bridge serving several delegated humans keeps one instance per sender —
 * the act-as identity is fixed at construction, mirroring clients that can
 * only set headers at connect time.
 */
export class MweClient {
  constructor(
    private readonly url: string,
    private readonly token: string,
    private readonly actAs = '',
    private readonly timeoutMs = 180_000,
  ) {}

  private headers(extra: Record<string, string> = {}): Record<string, string> {
    const headers: Record<string, string> = {
      Authorization: `Bearer ${this.token}`,
      // Streamable HTTP: the server may answer with a plain JSON body or an
      // SSE stream; advertise both.
      Accept: 'application/json, text/event-stream',
      ...extra,
    };
    if (this.actAs) headers['X-MWE-Act-As'] = this.actAs;
    return headers;
  }

  private async post(url: string, body: string | FormData, headers: Record<string, string>): Promise<Response> {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeoutMs);
    try {
      return await fetch(url, { method: 'POST', body, headers, signal: controller.signal });
    } finally {
      clearTimeout(timer);
    }
  }

  /** Call one MCP tool and return its payload. Throws `MweError` on any failure. */
  async callTool(tool: string, args: Record<string, unknown> = {}): Promise<Record<string, unknown>> {
    const response = await this.post(
      this.url,
      JSON.stringify({ jsonrpc: '2.0', id: 99, method: 'tools/call', params: { name: tool, arguments: args } }),
      this.headers({ 'Content-Type': 'application/json' }),
    );
    const text = await response.text();
    let parsed: Record<string, unknown>;
    try {
      parsed = JSON.parse(text) as Record<string, unknown>;
    } catch {
      throw new MweError(`${tool}: HTTP ${response.status}: ${text.slice(0, 200)}`);
    }
    const rpcError = parsed.error as { code?: number; message?: string; data?: unknown } | undefined;
    if (rpcError) {
      throw new MweError(`${tool}: JSON-RPC error ${rpcError.code}: ${rpcError.message}`, {
        code: String(rpcError.code ?? ''),
        data: rpcError.data,
      });
    }
    if (response.status >= 400) throw new MweError(`${tool}: HTTP ${response.status}: ${text.slice(0, 200)}`);

    const result = (parsed.result ?? {}) as { content?: Array<Record<string, unknown>>; isError?: boolean; structuredContent?: unknown };
    let payload: Record<string, unknown> | undefined;
    for (const item of result.content ?? []) {
      if (item.type !== 'text') continue;
      try {
        payload = JSON.parse(String(item.text)) as Record<string, unknown>;
      } catch {
        payload = { text: String(item.text) };
      }
      break;
    }
    if (payload === undefined && result.structuredContent) payload = result.structuredContent as Record<string, unknown>;
    if (result.isError) throw new MweError(`${tool}: tool error: ${JSON.stringify(payload)}`, { data: payload });
    if (payload === undefined) throw new MweError(`${tool}: no content in result`);
    return payload;
  }

  /**
   * Upload one media file to the out-of-band byte endpoint and return its
   * catalog row. The origin is the MCP url minus its trailing `/mcp`; the
   * bearer and act-as headers are the same as the MCP calls, so the upload is
   * attributed to the same effective principal.
   */
  async uploadMedia(upload: MediaUpload): Promise<Record<string, unknown>> {
    const mediaUrl = `${this.url.replace(/\/mcp\/?$/, '')}/media`;
    // Multipart headers are line-oriented: a quote or newline in the filename
    // would corrupt the part header.
    const filename = (upload.filename || 'blob').replace(/["\r\n]/g, '_');
    const form = new FormData();
    form.append('kind', upload.kind);
    if (upload.caption) form.append('caption', upload.caption);
    if (upload.description) form.append('description', upload.description);
    form.append('file', new Blob([upload.bytes], { type: upload.mime || 'application/octet-stream' }), filename);

    const response = await this.post(mediaUrl, form, this.headers());
    const text = await response.text();
    let parsed: Record<string, unknown>;
    try {
      parsed = JSON.parse(text) as Record<string, unknown>;
    } catch {
      throw new MweError(`media upload: HTTP ${response.status}: ${text.slice(0, 200)}`);
    }
    if (response.status >= 400 || typeof parsed.catalog_id !== 'string') {
      const err = (parsed.error ?? {}) as { code?: string; message?: string };
      throw new MweError(`media upload: HTTP ${response.status}: ${err.code}: ${err.message}`, {
        code: err.code,
        data: parsed,
      });
    }
    return parsed;
  }
}

/**
 * The `consumer_id` claim of the bearer token — "which consumer am I",
 * read from the same source of truth the server validates against.
 *
 * No signature check: the server enforces the claim on every call. Returns
 * an empty string for anything that is not a JWT carrying the claim, and the
 * events daemon then stays down rather than polling as nobody.
 */
export function consumerIdFromToken(token: string): string {
  try {
    const payload = token.split('.')[1] ?? '';
    const padded = payload + '='.repeat((4 - (payload.length % 4)) % 4);
    const claims = JSON.parse(Buffer.from(padded, 'base64url').toString('utf-8')) as Record<string, unknown>;
    return typeof claims.consumer_id === 'string' ? claims.consumer_id.trim() : '';
  } catch {
    return '';
  }
}
