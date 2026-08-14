import { api } from "./api";
import type { Artifact } from "./model";

export async function uploadArtifactFile(file: File, kind: Artifact["kind"]): Promise<Artifact> {
  const form = new FormData();
  form.append("file", file);
  return api<Artifact>(`/api/artifacts?kind=${kind}`, {
    method: "POST",
    body: form,
  });
}
