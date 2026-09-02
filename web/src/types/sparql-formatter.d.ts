declare module "sparql-formatter" {
  export const spfmt: {
    format(query: string, mode?: "default" | "compact" | "turtle" | "jsonld", indent?: number): string;
  };
}
