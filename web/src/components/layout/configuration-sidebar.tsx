"use client";

import { useEffect, type ReactNode } from "react";
import { usePathname, useRouter, useSearchParams } from "next/navigation";
import { ArrowLeftIcon, XMarkIcon } from "@heroicons/react/24/outline";
import * as Dialog from "@radix-ui/react-dialog";
import { useMobile } from "@/lib/use-mobile";
import { useNavigation } from "@/lib/navigation-context";

interface ConfigurationSidebarProps<Section extends string> {
  header: ReactNode;
  backHref: string;
  backLabel: string;
  sections: readonly { id: Section; label: string }[];
  activeSection: Section;
  onSectionChange: (section: Section) => void;
}

export function ConfigurationSidebar<Section extends string>({
  header,
  backHref,
  backLabel,
  sections,
  activeSection,
  onSectionChange,
}: ConfigurationSidebarProps<Section>) {
  const mobile = useMobile();
  const { mobileSubNavOpen, setMobileSubNavOpen } = useNavigation();
  const router = useRouter();
  const pathname = usePathname();
  const search = useSearchParams().toString();

  useEffect(() => {
    setMobileSubNavOpen(false);
  }, [pathname, search, setMobileSubNavOpen]);

  useEffect(() => {
    if (!mobile) setMobileSubNavOpen(false);
  }, [mobile, setMobileSubNavOpen]);

  const content = (
    <>
      <button
        type="button"
        onClick={() => {
          router.push(backHref);
          setMobileSubNavOpen(false);
        }}
        className="flex items-center gap-2 text-sm text-text-secondary hover:text-text-primary transition mb-4"
      >
        <ArrowLeftIcon className="h-4 w-4 shrink-0" aria-hidden="true" />
        {backLabel}
      </button>
      {header}
      <nav aria-label="Configuration sections" className="space-y-1">
        {sections.map(section => (
          <button
            key={section.id}
            type="button"
            aria-current={activeSection === section.id ? "page" : undefined}
            onClick={() => {
              onSectionChange(section.id);
              setMobileSubNavOpen(false);
            }}
            className={`w-full text-left rounded-lg px-3 py-2 text-sm transition ${activeSection === section.id
              ? "bg-accent/10 text-accent font-medium"
              : "text-text-secondary hover:bg-surface-tertiary hover:text-text-primary"}`}
          >
            {section.label}
          </button>
        ))}
      </nav>
    </>
  );

  if (!mobile) {
    return (
      <aside className="w-[289px] shrink-0 min-h-0 overflow-y-auto border-r border-border bg-surface-nav p-4">
        {content}
      </aside>
    );
  }

  return (
    <Dialog.Root open={mobileSubNavOpen} onOpenChange={setMobileSubNavOpen}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-black/40" />
        <Dialog.Content
          id="configuration-navigation"
          aria-describedby={undefined}
          onCloseAutoFocus={event => {
            // The shared top bar owns this drawer's trigger, outside this route.
            event.preventDefault();
            document.getElementById("mobile-navigation-toggle")?.focus();
          }}
          className="fixed inset-y-0 left-0 z-50 w-[85vw] max-w-[289px] min-h-0 overflow-y-auto border-r border-border bg-surface-nav p-4 shadow-xl"
        >
          <div className="mb-4 flex items-center justify-between gap-2">
            <Dialog.Title className="text-sm font-semibold">Configuration navigation</Dialog.Title>
            <Dialog.Close
              aria-label="Close configuration navigation"
              className="flex h-10 w-10 shrink-0 items-center justify-center rounded-lg hover:bg-surface-tertiary"
            >
              <XMarkIcon className="h-5 w-5" aria-hidden="true" />
            </Dialog.Close>
          </div>
          {content}
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
