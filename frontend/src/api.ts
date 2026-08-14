export interface ApiErrorBody {
  code: string;
  message: string;
}

const apiMessageTranslations: Record<string, string> = {
  "managed_direct requires profile_revision_id":
    "직접 연결을 사용하려면 준비된 네트워크 프로파일을 선택해야 합니다.",
  "server_port must be non-zero": "서버 포트는 1 이상이어야 합니다.",
  "explicit_proxy requires client and server node IDs":
    "명시적 프록시를 사용하려면 클라이언트와 서버 Agent를 선택해야 합니다.",
  "explicit_proxy endpoint addresses must be IPv4":
    "명시적 프록시의 클라이언트와 서버 주소는 IPv4여야 합니다.",
  "explicit_proxy requires server port and proxy host:port":
    "명시적 프록시를 사용하려면 서버 포트와 프록시 주소(host:port)가 필요합니다.",
  "PCAP session replay requires an analyzed capture artifact":
    "PCAP 세션 재현을 사용하려면 분석이 완료된 캡처 파일을 선택해야 합니다.",
  "HTTP/2 requires TLS; h2c is not supported":
    "HTTP/2를 사용하려면 TLS를 활성화해야 합니다. h2c는 지원하지 않습니다.",
  "HTTP/2 max_concurrent_streams must be between 1 and 1000":
    "HTTP/2 동시 스트림 수는 1~1,000 사이여야 합니다.",
  "network profile name is required": "네트워크 프로파일 이름을 입력해야 합니다.",
  "MTU must be between 576 and 9216": "MTU는 576~9,216 사이여야 합니다.",
  "diagnostic_port must be non-zero": "진단 포트는 1 이상이어야 합니다.",
  "client and server pools must use the same IPv4 subnet":
    "클라이언트와 서버 IP 풀은 같은 IPv4 서브넷을 사용해야 합니다.",
  "client and server pools must not overlap": "클라이언트와 서버 IP 풀은 서로 겹칠 수 없습니다.",
  "client and server endpoints require different interfaces on the same node":
    "같은 Agent에서 클라이언트와 서버를 구성하려면 서로 다른 인터페이스가 필요합니다.",
  "endpoint node and interface are required": "Agent와 네트워크 인터페이스를 선택해야 합니다.",
  "endpoint pool count must be between 1 and 4096": "IP 풀 개수는 1~4,096 사이여야 합니다.",
  "start_cidr must be IPv4/prefix": "시작 주소를 IPv4/CIDR 형식으로 입력해야 합니다.",
  "invalid IPv4 prefix": "올바른 IPv4 프리픽스를 입력해야 합니다.",
  "IPv4 prefix must be between 1 and 30": "IPv4 프리픽스는 1~30 사이여야 합니다.",
  "IP pool overflows": "IP 풀 범위가 허용 가능한 주소 범위를 초과합니다.",
  "IP pool includes network/broadcast or leaves subnet":
    "IP 풀에 네트워크·브로드캐스트 주소가 포함되었거나 서브넷 범위를 벗어났습니다.",
};

export function localizeApiMessage(message: string): string {
  const translated = apiMessageTranslations[message];
  if (translated) return translated;

  const payloadLimit = message.match(/^(request|response) payload exceeds 64 MiB$/);
  if (payloadLimit) {
    return `${payloadLimit[1] === "request" ? "요청" : "응답"} payload는 64MiB를 초과할 수 없습니다.`;
  }
  const fileArtifact = message.match(/^(request|response) file payload requires artifact_id$/);
  if (fileArtifact) {
    return `${fileArtifact[1] === "request" ? "요청" : "응답"} 파일 payload를 선택해야 합니다.`;
  }
  const unexpectedArtifact = message.match(
    /^(request|response) artifact_id is only valid for file payload$/,
  );
  if (unexpectedArtifact) {
    return `${unexpectedArtifact[1] === "request" ? "요청" : "응답"} artifact는 파일 payload에서만 사용할 수 있습니다.`;
  }
  if (message.startsWith("cipher suite ") && message.includes(" is not supported by ")) {
    return "선택한 cipher suite는 현재 TLS 버전에서 지원되지 않습니다.";
  }
  return message;
}

export class ApiClientError extends Error {
  readonly code: string;
  readonly status: number;

  constructor(status: number, body: Partial<ApiErrorBody>) {
    super(localizeApiMessage(body.message || `요청이 실패했습니다 (HTTP ${status})`));
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
      message:
        cause instanceof TypeError
          ? "서버에 연결할 수 없습니다. Control 서비스 상태를 확인해 주세요."
          : cause instanceof Error
            ? cause.message
            : "서버에 연결할 수 없습니다.",
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
