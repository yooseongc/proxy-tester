import { afterEach, describe, expect, it, vi } from "vitest";
import { ApiClientError, api, localizeApiMessage } from "./api";

afterEach(() => vi.unstubAllGlobals());

describe("api", () => {
  it("preserves structured API errors", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(
        new Response(JSON.stringify({ code: "invalid_scenario", message: "invalid payload" }), {
          status: 422,
          headers: { "content-type": "application/json" },
        }),
      ),
    );

    const error = await api("/api/scenarios").catch((value) => value);
    expect(error).toBeInstanceOf(ApiClientError);
    expect(error).toMatchObject({
      status: 422,
      code: "invalid_scenario",
      message: "invalid payload",
    });
  });

  it("classifies transport failures", async () => {
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new TypeError("connection refused")));

    await expect(api("/api/health")).rejects.toMatchObject({
      status: 0,
      code: "network_error",
      message: "서버에 연결할 수 없습니다. Control 서비스 상태를 확인해 주세요.",
    });
  });

  it("localizes known validation messages and preserves unknown details", () => {
    expect(localizeApiMessage("managed_direct requires profile_revision_id")).toBe(
      "직접 연결을 사용하려면 준비된 네트워크 프로파일을 선택해야 합니다.",
    );
    expect(localizeApiMessage("response payload exceeds 64 MiB")).toBe(
      "응답 payload는 64MiB를 초과할 수 없습니다.",
    );
    expect(localizeApiMessage("agent node-a is offline")).toBe("agent node-a is offline");
  });

  it("adds JSON content type without discarding caller headers", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      new Response(JSON.stringify({ ok: true }), {
        headers: { "content-type": "application/json" },
      }),
    );
    vi.stubGlobal("fetch", fetchMock);

    await api("/api/test", {
      method: "POST",
      headers: { "x-request-id": "request-1" },
      body: JSON.stringify({ value: 1 }),
    });

    const headers = fetchMock.mock.calls[0][1].headers as Headers;
    expect(headers.get("content-type")).toBe("application/json");
    expect(headers.get("x-request-id")).toBe("request-1");
  });
});
