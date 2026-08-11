export async function api<T>(path:string,init?:RequestInit):Promise<T>{
 const headers=typeof init?.body==='string'?{'content-type':'application/json'}:undefined;
 const response=await fetch(path,{headers,...init});
 const body=await response.json();
 if(!response.ok)throw new Error(body.error??response.statusText);
 return body;
}
