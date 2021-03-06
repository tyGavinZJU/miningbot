;; PoX testnet constants
;; Min/max number of reward cycles uSTX can be locked for
(define-constant MIN_POX_REWARD_CYCLES u1)
(define-constant MAX_POX_REWARD_CYCLES u12)

;; Default length of the PoX registration window, in burnchain blocks.
(define-constant PREPARE_CYCLE_LENGTH u30)

;; Default length of the PoX reward cycle, in burnchain blocks.
(define-constant REWARD_CYCLE_LENGTH u120)

;; Valid values for burnchain address versions.
;; These correspond to address hash modes in Stacks 2.0.
(define-constant ADDRESS_VERSION_P2PKH 0x00)
(define-constant ADDRESS_VERSION_P2SH 0x01)
(define-constant ADDRESS_VERSION_P2WPKH 0x02)
(define-constant ADDRESS_VERSION_P2WSH 0x03)

;; Stacking thresholds
(define-constant STACKING_THRESHOLD_25 u480)
(define-constant STACKING_THRESHOLD_100 u120)
