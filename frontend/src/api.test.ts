import { afterEach, describe, expect, it, vi } from "vitest";
import { ApiClientError, api } from "./api";

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
      message: "connection refused",
    });
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
