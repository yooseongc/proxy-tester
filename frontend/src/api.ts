export interface ApiErrorBody {
  code: string;
  message: string;
}

export class ApiClientError extends Error {
  readonly code: string;
  readonly status: number;

  constructor(status: number, body: Partial<ApiErrorBody>) {
    super(body.message || `요청이 실패했습니다 (HTTP ${status})`);
    this.name = "ApiClientError";
    this.code = body.code || "http_error";
    this.status = status;
  }
}

function isJson(response: Response): boolean {
  return response.headers.get("content-type")?.includes("application/json") ?? false;
}

export async function api<T = unknown>(path: string, init: RequestInit = {}): Promise<T> {
  const headers = new Headers(init.headers);
  if (typeof init.body === "string" && !headers.has("content-type")) {
    headers.set("content-type", "application/json");
  }

  let response: Response;
  try {
    response = await fetch(path, { ...init, headers });
  } catch (cause) {
    throw new ApiClientError(0, {
      code: "network_error",
      message: cause instanceof Error ? cause.message : "서버에 연결할 수 없습니다",
    });
  }

  const body = isJson(response)
    ? await response.json().catch(() => null)
    : await response.text().catch(() => "");
  if (!response.ok) {
    const error = body && typeof body === "object" ? body : { message: String(body) };
    throw new ApiClientError(response.status, error as Partial<ApiErrorBody>);
  }
  return body as T;
}
