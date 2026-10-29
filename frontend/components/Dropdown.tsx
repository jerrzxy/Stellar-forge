import React, {
  forwardRef,
  useCallback,
  useEffect,
  useId,
  useRef,
  useState,
} from 'react';

export type DropdownVariant = 'default' | 'primary' | 'danger';
export type DropdownSize = 'sm' | 'md' | 'lg';

export interface DropdownOption<T extends string = string> {
  value: T;
  label: string;
  disabled?: boolean;
  description?: string;
}

// Alias for convenience
export type DropdownItem<T extends string = string> = DropdownOption<T>;

export interface DropdownProps<T extends string = string>
  extends Omit<React.HTMLAttributes<HTMLDivElement>, 'onChange' | 'defaultValue'> {
  /** List of selectable options */
  options?: DropdownOption<T>[];
  /** Alternative alias for options */
  items?: DropdownOption<T>[];
  /** Controlled selected value */
  value?: T;
  /** Uncontrolled initial value */
  defaultValue?: T;
  /** Callback invoked when selection changes with value */
  onChange?: (value: T) => void;
  /** Callback invoked when selection changes with full option */
  onSelect?: (option: DropdownOption<T>) => void;
  /** Accessible label for the dropdown button / combobox */
  label?: string;
  /** Visible label text rendered above the dropdown */
  visibleLabel?: string;
  /** Placeholder text shown when no item is selected */
  placeholder?: string;
  /** Whether the dropdown is disabled */
  disabled?: boolean;
  /** Visual variant */
  variant?: DropdownVariant;
  /** Size variant */
  size?: DropdownSize;
  /** ID for trigger button */
  id?: string;
}

const SIZE_STYLES: Record<DropdownSize, React.CSSProperties> = {
  sm: { fontSize: '12px', padding: '4px 8px', minHeight: '28px' },
  md: { fontSize: '14px', padding: '8px 12px', minHeight: '36px' },
  lg: { fontSize: '16px', padding: '10px 16px', minHeight: '44px' },
};

const VARIANT_TRIGGER: Record<DropdownVariant, React.CSSProperties> = {
  default: {
    backgroundColor: '#ffffff',
    borderColor: '#d1d5db',
    color: '#111827',
  },
  primary: {
    backgroundColor: '#2563eb',
    borderColor: '#2563eb',
    color: '#ffffff',
  },
  danger: {
    backgroundColor: '#dc2626',
    borderColor: '#dc2626',
    color: '#ffffff',
  },
};

/**
 * Accessible Dropdown Component with dynamic WAI-ARIA attributes.
 *
 * Ensures full WCAG accessibility compliance by providing explicit
 * dynamic `aria-expanded` attributes on the toggle trigger component.
 */
export const Dropdown = forwardRef<HTMLDivElement, DropdownProps>(function Dropdown(
  {
    options,
    items,
    value,
    defaultValue,
    onChange,
    onSelect,
    label,
    visibleLabel,
    placeholder = 'Select...',
    disabled = false,
    variant = 'default',
    size = 'md',
    id: customId,
    className = '',
    style,
    ...rest
  },
  ref,
) {
  const generatedId = useId();
  const triggerId = customId || `dropdown-trigger-${generatedId}`;
  const menuId = `dropdown-menu-${generatedId}`;

  const optionList: DropdownOption[] = options || items || [];

  const [isOpen, setIsOpen] = useState(false);
  const [internalValue, setInternalValue] = useState<string>(defaultValue ?? '');
  const [activeIndex, setActiveIndex] = useState<number>(-1);

  const isControlled = value !== undefined;
  const currentValue = isControlled ? value : internalValue;

  const containerRef = useRef<HTMLDivElement | null>(null);
  const triggerRef = useRef<HTMLButtonElement | null>(null);
  const menuRef = useRef<HTMLUListElement | null>(null);

  const selectedOption = optionList.find((opt) => opt.value === currentValue);

  // Close when clicking outside
  useEffect(() => {
    if (!isOpen) return;

    const handleOutsideClick = (e: MouseEvent | TouchEvent) => {
      if (
        containerRef.current &&
        !containerRef.current.contains(e.target as Node)
      ) {
        setIsOpen(false);
        setActiveIndex(-1);
      }
    };

    document.addEventListener('mousedown', handleOutsideClick);
    document.addEventListener('touchstart', handleOutsideClick);
    return () => {
      document.removeEventListener('mousedown', handleOutsideClick);
      document.removeEventListener('touchstart', handleOutsideClick);
    };
  }, [isOpen]);

  const handleSelect = useCallback(
    (option: DropdownOption) => {
      if (option.disabled || disabled) return;

      if (!isControlled) {
        setInternalValue(option.value);
      }

      onChange?.(option.value);
      onSelect?.(option);
      setIsOpen(false);
      setActiveIndex(-1);
      triggerRef.current?.focus();
    },
    [disabled, isControlled, onChange, onSelect],
  );

  const handleTriggerClick = () => {
    if (disabled) return;
    setIsOpen((prev) => {
      const next = !prev;
      if (next) {
        const selectedIdx = optionList.findIndex((opt) => opt.value === currentValue);
        setActiveIndex(selectedIdx >= 0 ? selectedIdx : 0);
      } else {
        setActiveIndex(-1);
      }
      return next;
    });
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (disabled) return;

    if (!isOpen) {
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp' || e.key === 'Enter' || e.key === ' ') {
        e.preventDefault();
        setIsOpen(true);
        const selectedIdx = optionList.findIndex((opt) => opt.value === currentValue);
        setActiveIndex(selectedIdx >= 0 ? selectedIdx : 0);
      }
      return;
    }

    switch (e.key) {
      case 'Escape':
        e.preventDefault();
        setIsOpen(false);
        setActiveIndex(-1);
        triggerRef.current?.focus();
        break;

      case 'ArrowDown':
        e.preventDefault();
        setActiveIndex((prev) => {
          for (let i = prev + 1; i < optionList.length; i++) {
            if (!optionList[i].disabled) return i;
          }
          // Wrap around
          for (let i = 0; i <= prev; i++) {
            if (!optionList[i].disabled) return i;
          }
          return prev;
        });
        break;

      case 'ArrowUp':
        e.preventDefault();
        setActiveIndex((prev) => {
          for (let i = prev - 1; i >= 0; i--) {
            if (!optionList[i].disabled) return i;
          }
          // Wrap around
          for (let i = optionList.length - 1; i >= prev; i--) {
            if (!optionList[i].disabled) return i;
          }
          return prev;
        });
        break;

      case 'Home':
        e.preventDefault();
        for (let i = 0; i < optionList.length; i++) {
          if (!optionList[i].disabled) {
            setActiveIndex(i);
            break;
          }
        }
        break;

      case 'End':
        e.preventDefault();
        for (let i = optionList.length - 1; i >= 0; i--) {
          if (!optionList[i].disabled) {
            setActiveIndex(i);
            break;
          }
        }
        break;

      case 'Enter':
      case ' ':
        e.preventDefault();
        if (activeIndex >= 0 && activeIndex < optionList.length) {
          handleSelect(optionList[activeIndex]);
        }
        break;

      case 'Tab':
        setIsOpen(false);
        setActiveIndex(-1);
        break;

      default:
        break;
    }
  };

  const activeDescendantId =
    isOpen && activeIndex >= 0 ? `${menuId}-option-${activeIndex}` : undefined;

  return (
    <div
      ref={(node) => {
        containerRef.current = node;
        if (typeof ref === 'function') {
          ref(node);
        } else if (ref && typeof ref === 'object') {
          (ref as React.MutableRefObject<HTMLDivElement | null>).current = node;
        }
      }}
      className={`dropdown-container ${className}`.trim()}
      style={{
        position: 'relative',
        display: 'inline-block',
        width: '100%',
        maxWidth: '320px',
        fontFamily: 'inherit',
        ...style,
      }}
      {...rest}
    >
      {visibleLabel && (
        <label
          htmlFor={triggerId}
          style={{
            display: 'block',
            marginBottom: '4px',
            fontSize: '13px',
            fontWeight: 500,
            color: '#374151',
          }}
        >
          {visibleLabel}
        </label>
      )}

      {/* Dropdown Toggle Component with Explicit Dynamic aria-expanded */}
      <button
        ref={triggerRef}
        id={triggerId}
        type="button"
        role="combobox"
        aria-haspopup="listbox"
        aria-expanded={isOpen}
        aria-controls={menuId}
        aria-activedescendant={activeDescendantId}
        aria-label={label || visibleLabel || placeholder}
        aria-disabled={disabled}
        disabled={disabled}
        onClick={handleTriggerClick}
        onKeyDown={handleKeyDown}
        style={{
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'space-between',
          width: '100%',
          border: '1px solid',
          borderRadius: '6px',
          cursor: disabled ? 'not-allowed' : 'pointer',
          opacity: disabled ? 0.6 : 1,
          boxSizing: 'border-box',
          outline: 'none',
          boxShadow: isOpen ? '0 0 0 2px rgba(37, 99, 235, 0.2)' : 'none',
          ...SIZE_STYLES[size],
          ...VARIANT_TRIGGER[variant],
        }}
      >
        <span
          style={{
            overflow: 'hidden',
            textOverflow: 'ellipsis',
            whiteSpace: 'nowrap',
          }}
        >
          {selectedOption ? selectedOption.label : placeholder}
        </span>
        <span
          aria-hidden="true"
          style={{
            marginLeft: '8px',
            transition: 'transform 0.15s ease',
            transform: isOpen ? 'rotate(180deg)' : 'rotate(0deg)',
          }}
        >
          ▾
        </span>
      </button>

      {/* Dropdown Menu / Listbox */}
      {isOpen && (
        <ul
          ref={menuRef}
          id={menuId}
          role="listbox"
          aria-label={label || visibleLabel || placeholder}
          style={{
            position: 'absolute',
            top: '100%',
            left: 0,
            right: 0,
            marginTop: '4px',
            padding: '4px 0',
            backgroundColor: '#ffffff',
            border: '1px solid #e5e7eb',
            borderRadius: '6px',
            boxShadow: '0 4px 6px -1px rgba(0, 0, 0, 0.1), 0 2px 4px -1px rgba(0, 0, 0, 0.06)',
            maxHeight: '240px',
            overflowY: 'auto',
            zIndex: 1000,
            listStyle: 'none',
            marginBlock: 0,
            paddingInlineStart: 0,
          }}
        >
          {optionList.length === 0 ? (
            <li
              style={{
                padding: '8px 12px',
                color: '#6b7280',
                fontSize: '13px',
              }}
            >
              No options available
            </li>
          ) : (
            optionList.map((opt, index) => {
              const isSelected = opt.value === currentValue;
              const isActive = index === activeIndex;

              return (
                <li
                  key={opt.value}
                  id={`${menuId}-option-${index}`}
                  role="option"
                  aria-selected={isSelected}
                  aria-disabled={opt.disabled}
                  onClick={() => handleSelect(opt)}
                  onMouseEnter={() => {
                    if (!opt.disabled) setActiveIndex(index);
                  }}
                  style={{
                    padding: '8px 12px',
                    fontSize: '14px',
                    color: opt.disabled ? '#9ca3af' : isSelected ? '#2563eb' : '#1f2937',
                    backgroundColor: isActive
                      ? '#f3f4f6'
                      : isSelected
                      ? '#eff6ff'
                      : 'transparent',
                    cursor: opt.disabled ? 'not-allowed' : 'pointer',
                    display: 'flex',
                    alignItems: 'center',
                    justifyContent: 'space-between',
                  }}
                >
                  <span>{opt.label}</span>
                  {isSelected && (
                    <span aria-hidden="true" style={{ color: '#2563eb', fontWeight: 600 }}>
                      ✓
                    </span>
                  )}
                </li>
              );
            })
          )}
        </ul>
      )}
    </div>
  );
});

export default Dropdown;
