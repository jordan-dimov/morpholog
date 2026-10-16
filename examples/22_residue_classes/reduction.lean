/-
The lemma the record cannot check for itself, stated over all integers.

The record verifies, by exhaustion, two statements about the residues
modulo 4. Carrying those statements to every integer needs this much:
reducing modulo m respects squaring and adding, and the parity of an
integer is the parity of its residue modulo 4. Lean's core library
proves each line; nothing beyond core Lean 4 is used.
-/

theorem square_reduces (n m : Int) :
    n * n % m = (n % m) * (n % m) % m :=
  Int.mul_emod n n m

theorem sum_of_squares_reduces (a b m : Int) :
    (a * a + b * b) % m = ((a % m) * (a % m) + (b % m) * (b % m)) % m := by
  rw [Int.add_emod, Int.mul_emod a a m, Int.mul_emod b b m, ← Int.add_emod]

theorem parity_reduces_mod_four (n : Int) :
    n % 2 = (n % 4) % 2 :=
  (Int.emod_emod_of_dvd n (by decide : (2 : Int) ∣ 4)).symm
